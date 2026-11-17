use std::fmt::Write as _;
use std::sync::atomic::{AtomicU8, Ordering};

use gossamer_lex::{FileId, TokenKind, tokenize};

/// Which IANA zones a compiled program carries for hosts with no zoneinfo
/// tree: the project's `tzdata`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EmbeddedZones {
    /// Every zone the program spells exactly in a string literal.
    #[default]
    Named,
    /// The whole database.
    All,
    /// None.
    None,
}

static EMBEDDED_ZONES: AtomicU8 = AtomicU8::new(0);

/// Sets which zones the programs this process compiles embed.
pub fn set_embedded_zones(zones: EmbeddedZones) {
    let code = match zones {
        EmbeddedZones::Named => 0,
        EmbeddedZones::All => 1,
        EmbeddedZones::None => 2,
    };
    EMBEDDED_ZONES.store(code, Ordering::Relaxed);
}

fn embedded_zones() -> EmbeddedZones {
    match EMBEDDED_ZONES.load(Ordering::Relaxed) {
        1 => EmbeddedZones::All,
        2 => EmbeddedZones::None,
        _ => EmbeddedZones::Named,
    }
}

/// The `__gos_time_zone_data` wrapper for `source`: one `name\tTZif\n` line
/// per embedded zone, the `TZif` data in base64. The runtime reads a zone from
/// it only when the host has none of its own.
pub(super) fn time_zone_data_wrapper(source: &str) -> String {
    zone_data_wrapper(source, embedded_zones())
}

fn zone_data_wrapper(source: &str, zones: EmbeddedZones) -> String {
    let names = match zones {
        EmbeddedZones::None => Vec::new(),
        EmbeddedZones::All => jiff_tzdb::available().collect(),
        EmbeddedZones::Named => named_zones(source),
    };
    // Written as the body of a string literal: the separators are escapes.
    let mut data = String::new();
    for name in names {
        if let Some((_, tzif)) = jiff_tzdb::get(name) {
            data.push_str(name);
            data.push_str("\\t");
            encode_base64(tzif, &mut data);
            data.push_str("\\n");
        }
    }
    let mut out = String::with_capacity(data.len() + 64);
    let _ = writeln!(out, "fn __gos_time_zone_data() -> String {{ \"{data}\" }}");
    out
}

/// The zones `source` spells in its string literals, in the database's own
/// spelling, without repeats. A literal that only names a zone in another
/// case is not one: a lookup matches the spelling exactly.
fn named_zones(source: &str) -> Vec<&'static str> {
    let (tokens, _) = tokenize(source, FileId::default());
    let mut names: Vec<&'static str> = tokens
        .iter()
        .filter(|token| token.kind == TokenKind::StringLit)
        .filter_map(|token| {
            let text = source.get(token.span.start as usize..token.span.end as usize)?;
            let body = text.strip_prefix('"')?.strip_suffix('"')?;
            jiff_tzdb::get(body)
                .map(|(canonical, _)| canonical)
                .filter(|canonical| *canonical == body)
        })
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// Appends `bytes` as standard base64 with padding.
fn encode_base64(bytes: &[u8], out: &mut String) {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> shift) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        let mut out = String::new();
        for (input, expected) in [
            (&b""[..], ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
        ] {
            out.clear();
            encode_base64(input, &mut out);
            assert_eq!(out, expected);
        }
    }

    #[test]
    fn each_mode_embeds_its_zones() {
        let source = r#"fn main() { let z = "Asia/Tokyo" }"#;
        let lines = |zones| {
            let wrapper = zone_data_wrapper(source, zones);
            let body = wrapper.split('"').nth(1).unwrap_or("").to_string();
            body.split("\\n")
                .filter(|line| !line.is_empty())
                .map(|line| line.split("\\t").next().unwrap_or("").to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(lines(EmbeddedZones::Named), vec!["Asia/Tokyo"]);
        assert!(lines(EmbeddedZones::None).is_empty());
        let all = lines(EmbeddedZones::All);
        assert_eq!(all.len(), jiff_tzdb::available().count());
        assert!(all.iter().any(|name| name == "Australia/Sydney"));
        assert!(
            zone_data_wrapper(source, EmbeddedZones::None)
                .starts_with("fn __gos_time_zone_data() -> String { \"\" }")
        );
    }

    #[test]
    fn named_zones_are_the_exact_spellings_in_string_literals() {
        let source = r#"
            const HOME: String = "Europe/Paris"
            fn main() {
                let a = "America/New_York"
                let b = "america/new_york"
                let c = "America/New_York"
                // "Asia/Tokyo" in a comment is not a literal
                let d = "not a zone"
            }
        "#;
        assert_eq!(
            named_zones(source),
            vec!["America/New_York", "Europe/Paris"]
        );
    }
}
