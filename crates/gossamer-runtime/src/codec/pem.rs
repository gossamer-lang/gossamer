//! PEM blocks (RFC 7468): a labelled Base64 body between `-----BEGIN` and
//! `-----END` lines.

use super::base64;

/// One decoded PEM block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// The label, such as `CERTIFICATE`.
    pub label: String,
    /// The decoded body.
    pub bytes: Vec<u8>,
}

/// PEM text of one block, its body wrapped at 64 characters.
#[must_use]
pub fn encode(label: &str, bytes: &[u8]) -> String {
    let body = base64::encode(bytes);
    let mut out = String::with_capacity(body.len() + body.len() / 64 + 2 * label.len() + 34);
    out.push_str("-----BEGIN ");
    out.push_str(label);
    out.push_str("-----\n");
    for line in body.as_bytes().chunks(64) {
        // Base64 text is ASCII, so every 64-byte cut is a character boundary.
        out.push_str(std::str::from_utf8(line).unwrap_or_default());
        out.push('\n');
    }
    out.push_str("-----END ");
    out.push_str(label);
    out.push_str("-----\n");
    out
}

/// The first block in `input` and the text after its END line.
pub fn decode(input: &str) -> Result<(Block, &str), String> {
    next_block(input)?.ok_or_else(|| "pem: no PEM data found".to_string())
}

/// Every block in `input`, in order.
pub fn decode_all(input: &str) -> Result<Vec<Block>, String> {
    let mut blocks = Vec::new();
    let mut remaining = input;
    while let Some((block, rest)) = next_block(remaining)? {
        blocks.push(block);
        remaining = rest;
    }
    Ok(blocks)
}

/// The next block in `input`, if a BEGIN line starts one, and the text after
/// its END line.
fn next_block(input: &str) -> Result<Option<(Block, &str)>, String> {
    const BEGIN: &str = "-----BEGIN ";
    const DASHES: &str = "-----";
    let Some(begin) = input.find(BEGIN) else {
        return Ok(None);
    };
    let rest = &input[begin + BEGIN.len()..];
    let end_label = rest.find(DASHES).ok_or("pem: malformed BEGIN line")?;
    let label = &rest[..end_label];
    let after_begin = &rest[end_label + DASHES.len()..];
    let end_marker = format!("-----END {label}-----");
    let end_pos = after_begin
        .find(end_marker.as_str())
        .ok_or_else(|| format!("pem: missing END {label}"))?;
    let body: String = after_begin[..end_pos]
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let bytes = base64::decode(&body).map_err(|e| format!("pem: base64 decode: {e}"))?;
    let block = Block {
        label: label.to_string(),
        bytes,
    };
    Ok(Some((block, &after_begin[end_pos + end_marker.len()..])))
}

#[cfg(test)]
mod tests {
    use super::{decode, decode_all, encode};

    #[test]
    fn blocks_round_trip() {
        let data: Vec<u8> = (0..=200u8).collect();
        let text = encode("TEST", &data) + &encode("OTHER", b"x");
        let (first, rest) = decode(&text).expect("decodes");
        assert_eq!(
            (first.label.as_str(), first.bytes.as_slice()),
            ("TEST", data.as_slice())
        );
        assert!(rest.starts_with('\n'));
        let all = decode_all(&text).expect("decodes all");
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].bytes, b"x");
    }

    #[test]
    fn malformed_blocks_are_refused() {
        assert!(decode("nothing here").is_err());
        assert_eq!(decode_all("nothing here"), Ok(Vec::new()));
        assert!(decode("-----BEGIN X-----\nZm9v\n").is_err());
        assert!(decode("-----BEGIN X-----\nZm9*\n-----END X-----").is_err());
        assert!(decode("-----BEGIN X\nZm9v").is_err());
    }
}
