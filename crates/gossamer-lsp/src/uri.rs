//! `file://` URIs for local paths, as editors send and expect them.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// The `file://` URI naming the absolute path `path`, with every byte
/// outside the unreserved set and `/` percent-encoded.
#[must_use]
pub fn path_to_uri(path: &Path) -> String {
    let text = path.to_string_lossy();
    // Only Windows separates components with `\`, and `canonicalize`
    // answers its drive paths in the verbatim `\\?\C:\` form.
    let text = if cfg!(windows) {
        let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
        text.replace('\\', "/")
    } else {
        text.into_owned()
    };
    let mut out = String::with_capacity(text.len() + 8);
    out.push_str("file://");
    // A Windows drive path (`C:/..`) has no leading slash; the URI's path
    // component must start with one.
    if !text.starts_with('/') {
        out.push('/');
    }
    for (index, byte) in text.bytes().enumerate() {
        let keep = byte.is_ascii_alphanumeric()
            || matches!(byte, b'/' | b'-' | b'.' | b'_' | b'~')
            || (byte == b':' && index == 1 && text.as_bytes()[0].is_ascii_alphabetic());
        if keep {
            out.push(char::from(byte));
        } else {
            // Writing into a `String` cannot fail.
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// The local path a `file://` URI names, undoing percent escapes. `None`
/// for another scheme, a remote authority, a malformed escape, or a path
/// that is not UTF-8.
#[must_use]
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }
    let bytes = rest.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        let hex = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
        let byte = u8::from_str_radix(hex, 16).ok()?;
        if byte == 0 {
            return None;
        }
        decoded.push(byte);
        index += 3;
    }
    let text = String::from_utf8(decoded).ok()?;
    Some(PathBuf::from(native_path(text)))
}

/// `/C:/dir` is the URI spelling of the Windows path `C:/dir`.
#[cfg(windows)]
fn native_path(text: String) -> String {
    let bytes = text.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        text[1..].to_string()
    } else {
        text
    }
}

/// Every other platform spells a local path as the URI's path component.
#[cfg(not(windows))]
fn native_path(text: String) -> String {
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_with_spaces_and_unicode_round_trips() {
        let path = Path::new("/tmp/a project/\u{3c0}#1.gos");
        let uri = path_to_uri(path);
        assert_eq!(uri, "file:///tmp/a%20project/%CF%80%231.gos");
        assert_eq!(uri_to_path(&uri).as_deref(), Some(path));
    }

    #[test]
    fn editor_spellings_decode_to_the_same_path() {
        assert_eq!(
            uri_to_path("file:///tmp/a%20project/%cf%80.gos"),
            Some(PathBuf::from("/tmp/a project/\u{3c0}.gos"))
        );
        assert_eq!(
            uri_to_path("file://localhost/tmp/x.gos"),
            Some(PathBuf::from("/tmp/x.gos"))
        );
    }

    #[test]
    fn malformed_and_foreign_uris_name_no_path() {
        assert_eq!(uri_to_path("file:///tmp/bad%2"), None);
        assert_eq!(uri_to_path("file:///tmp/nul%00"), None);
        assert_eq!(uri_to_path("file://server/share/a.gos"), None);
        assert_eq!(uri_to_path("https://example.com/a.gos"), None);
        assert_eq!(uri_to_path("inmemory://model/1"), None);
    }

    #[cfg(windows)]
    #[test]
    fn a_drive_path_gains_a_leading_slash() {
        assert_eq!(
            path_to_uri(Path::new("C:\\work dir\\main.gos")),
            "file:///C:/work%20dir/main.gos"
        );
        assert_eq!(
            path_to_uri(Path::new(r"\\?\C:\work\main.gos")),
            "file:///C:/work/main.gos"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_drive_uri_decodes_to_a_drive_path() {
        assert_eq!(
            uri_to_path("file:///c%3A/work/main.gos"),
            Some(PathBuf::from("c:/work/main.gos"))
        );
    }
}
