//! Runtime support for `std::encoding::{base64, hex, binary, yaml}`.
//! Pure-Rust, allocation-conscious one-shot encode/decode helpers.
//! The `binary` submodule wraps endianness packing, `base64` and
//! `hex` handle byte-string conversion, and `yaml` provides a
//! general-purpose YAML 1.2 parser/emitter (gated on the `yaml`
//! feature).

#![forbid(unsafe_code)]

/// Adobe ASCII85 / btoa encoding.
pub mod ascii85;
/// RFC 4648 Base32 (standard and hex alphabets).
pub mod base32;
/// XML parsing and encoding via quick-xml.
pub mod xml;
pub mod yaml;

pub mod base64 {
    //! RFC 4648 base64 with the standard alphabet.

    use crate::errors::Error;

    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    /// Encodes `input` to a base64 string (with `=` padding).
    #[must_use]
    pub fn encode(input: &[u8]) -> String {
        let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
        let mut chunks = input.chunks_exact(3);
        for chunk in chunks.by_ref() {
            let n = (u32::from(chunk[0]) << 16) | (u32::from(chunk[1]) << 8) | u32::from(chunk[2]);
            out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
            out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
            out.push(ALPHABET[(n & 0x3f) as usize] as char);
        }
        let rem = chunks.remainder();
        match rem.len() {
            1 => {
                let n = u32::from(rem[0]) << 16;
                out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
                out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
                out.push('=');
                out.push('=');
            }
            2 => {
                let n = (u32::from(rem[0]) << 16) | (u32::from(rem[1]) << 8);
                out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
                out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
                out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
                out.push('=');
            }
            _ => {}
        }
        out
    }

    /// Decodes a base64 string, tolerating whitespace between characters.
    /// The input is whole groups of four; `=` pads only the last group,
    /// as `xx==` or `xxx=`.
    pub fn decode(input: &str) -> Result<Vec<u8>, Error> {
        let filtered: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
        if !filtered.len().is_multiple_of(4) {
            return Err(Error::new("base64: input length must be a multiple of 4"));
        }
        let pad = filtered.iter().rev().take_while(|b| **b == b'=').count();
        let data = filtered.len() - pad;
        if filtered[..data].contains(&b'=') {
            return Err(Error::new("base64: data after padding"));
        }
        let valid_padding = match pad {
            0 => true,
            1 => data % 4 == 3,
            2 => data % 4 == 2,
            _ => false,
        };
        if !valid_padding {
            return Err(Error::new("base64: padding does not end a group"));
        }
        let mut out = Vec::with_capacity(filtered.len() / 4 * 3);
        for chunk in filtered.chunks(4) {
            let mut values = [0u32; 4];
            let mut chunk_pad = 0;
            for (i, byte) in chunk.iter().enumerate() {
                if *byte == b'=' {
                    chunk_pad += 1;
                } else {
                    values[i] = index(*byte)
                        .ok_or_else(|| {
                            Error::new(format!("base64: invalid character '{}'", *byte as char))
                        })?
                        .into();
                }
            }
            let n = (values[0] << 18) | (values[1] << 12) | (values[2] << 6) | values[3];
            out.push((n >> 16) as u8);
            if chunk_pad < 2 {
                out.push((n >> 8) as u8);
            }
            if chunk_pad < 1 {
                out.push(n as u8);
            }
        }
        Ok(out)
    }

    fn index(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
}

pub mod hex {
    //! Lowercase hex encoding.

    use crate::errors::Error;

    /// Encodes `input` as lowercase hex.
    #[must_use]
    pub fn encode(input: &[u8]) -> String {
        let mut out = String::with_capacity(input.len() * 2);
        for byte in input {
            out.push(nibble(*byte >> 4));
            out.push(nibble(*byte & 0xf));
        }
        out
    }

    /// Decodes a hex string, rejecting non-hex bytes and odd length.
    pub fn decode(input: &str) -> Result<Vec<u8>, Error> {
        if !input.len().is_multiple_of(2) {
            return Err(Error::new("hex input must have even length"));
        }
        let bytes = input.as_bytes();
        let mut out = Vec::with_capacity(bytes.len() / 2);
        for pair in bytes.chunks(2) {
            let hi = value(pair[0]).ok_or_else(|| Error::new("bad hex digit"))?;
            let lo = value(pair[1]).ok_or_else(|| Error::new("bad hex digit"))?;
            out.push((hi << 4) | lo);
        }
        Ok(out)
    }

    const fn nibble(n: u8) -> char {
        match n {
            0..=9 => (b'0' + n) as char,
            10..=15 => (b'a' + n - 10) as char,
            _ => '?',
        }
    }

    fn value(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }
}

pub mod binary {
    //! Endianness helpers and variable-length integer encoding.

    use crate::errors::Error;

    // ----- u8 -----

    /// Reads a single byte from `input[0]`.
    #[must_use]
    pub fn get_u8(input: &[u8]) -> u8 {
        input[0]
    }

    /// Writes `value` into `out[0]`.
    pub fn put_u8(out: &mut [u8], value: u8) {
        out[0] = value;
    }

    // ----- u16 -----

    /// Writes `value` big-endian into `out[..2]`. Panics if `out` is
    /// too small.
    pub fn put_u16_be(out: &mut [u8], value: u16) {
        out[..2].copy_from_slice(&value.to_be_bytes());
    }

    /// Writes `value` little-endian into `out[..2]`.
    pub fn put_u16_le(out: &mut [u8], value: u16) {
        out[..2].copy_from_slice(&value.to_le_bytes());
    }

    /// Reads a big-endian `u16` from `input[..2]`.
    #[must_use]
    pub fn get_u16_be(input: &[u8]) -> u16 {
        u16::from_be_bytes([input[0], input[1]])
    }

    /// Reads a little-endian `u16` from `input[..2]`.
    #[must_use]
    pub fn get_u16_le(input: &[u8]) -> u16 {
        u16::from_le_bytes([input[0], input[1]])
    }

    // ----- u32 -----

    /// Writes `value` big-endian into `out[..4]`.
    pub fn put_u32_be(out: &mut [u8], value: u32) {
        out[..4].copy_from_slice(&value.to_be_bytes());
    }

    /// Writes `value` little-endian into `out[..4]`.
    pub fn put_u32_le(out: &mut [u8], value: u32) {
        out[..4].copy_from_slice(&value.to_le_bytes());
    }

    /// Reads a big-endian `u32` from `input[..4]`.
    #[must_use]
    pub fn get_u32_be(input: &[u8]) -> u32 {
        u32::from_be_bytes([input[0], input[1], input[2], input[3]])
    }

    /// Reads a little-endian `u32` from `input[..4]`.
    #[must_use]
    pub fn get_u32_le(input: &[u8]) -> u32 {
        u32::from_le_bytes([input[0], input[1], input[2], input[3]])
    }

    // ----- u64 -----

    /// Writes `value` big-endian into `out[..8]`.
    pub fn put_u64_be(out: &mut [u8], value: u64) {
        out[..8].copy_from_slice(&value.to_be_bytes());
    }

    /// Writes `value` little-endian into `out[..8]`.
    pub fn put_u64_le(out: &mut [u8], value: u64) {
        out[..8].copy_from_slice(&value.to_le_bytes());
    }

    /// Reads a big-endian `u64` from `input[..8]`.
    #[must_use]
    pub fn get_u64_be(input: &[u8]) -> u64 {
        u64::from_be_bytes([
            input[0], input[1], input[2], input[3], input[4], input[5], input[6], input[7],
        ])
    }

    /// Reads a little-endian `u64` from `input[..8]`.
    #[must_use]
    pub fn get_u64_le(input: &[u8]) -> u64 {
        u64::from_le_bytes([
            input[0], input[1], input[2], input[3], input[4], input[5], input[6], input[7],
        ])
    }

    // ----- varint (LEB128-style, Go-compatible) -----

    /// Encodes `x` as an unsigned varint into `buf`.
    /// Returns the number of bytes written.
    pub fn put_uvarint(buf: &mut [u8], x: u64) -> usize {
        let mut n = 0;
        let mut v = x;
        while v >= 0x80 {
            buf[n] = (v as u8) | 0x80;
            v >>= 7;
            n += 1;
        }
        buf[n] = v as u8;
        n + 1
    }

    /// Encodes `x` as a signed varint using zigzag encoding.
    /// Returns the number of bytes written.
    pub fn put_varint(buf: &mut [u8], x: i64) -> usize {
        let ux = if x >= 0 {
            (x as u64) << 1
        } else {
            (!(x as u64) << 1) | 1
        };
        put_uvarint(buf, ux)
    }

    /// Decodes an unsigned varint from `buf`.
    /// Returns `(value, bytes_consumed)` or an error.
    pub fn uvarint(buf: &[u8]) -> Result<(u64, usize), Error> {
        let mut x = 0u64;
        let mut s = 0u32;
        for (i, &b) in buf.iter().enumerate() {
            if i == 10 {
                return Err(Error::new("varint overflows u64"));
            }
            if b < 0x80 {
                if i == 9 && b > 1 {
                    return Err(Error::new("varint overflows u64"));
                }
                return Ok((x | (u64::from(b) << s), i + 1));
            }
            x |= u64::from(b & 0x7f) << s;
            s += 7;
        }
        Err(Error::new("varint: buffer too small"))
    }

    /// Decodes a signed varint (zigzag) from `buf`.
    /// Returns `(value, bytes_consumed)` or an error.
    pub fn varint(buf: &[u8]) -> Result<(i64, usize), Error> {
        let (ux, n) = uvarint(buf)?;
        let x = if ux & 1 == 0 {
            (ux >> 1) as i64
        } else {
            !((ux >> 1) as i64)
        };
        Ok((x, n))
    }
}

/// CSV reading and writing.
pub mod csv {
    use crate::errors::Error;

    /// Parses a single CSV-formatted line, respecting double-quoted fields
    /// and escaped quotes (`""`).
    #[must_use]
    pub fn parse_line(line: &str) -> Vec<String> {
        let mut fields = Vec::new();
        let mut field = String::new();
        let mut in_quotes = false;
        let mut chars = line.chars().peekable();

        while let Some(c) = chars.next() {
            match c {
                '"' if in_quotes => {
                    if chars.peek() == Some(&'"') {
                        field.push('"');
                        chars.next();
                    } else {
                        in_quotes = false;
                    }
                }
                '"' => {
                    in_quotes = true;
                }
                ',' if !in_quotes => {
                    fields.push(field.clone());
                    field.clear();
                }
                _ => field.push(c),
            }
        }
        fields.push(field);
        fields
    }

    /// Parses all records from a CSV string.  Each record is a `Vec<String>`.
    /// Empty lines are skipped.  Returns an error if a quoted field is
    /// never closed.
    pub fn read(input: &str) -> Result<Vec<Vec<String>>, Error> {
        let mut records = Vec::new();
        for line in input.lines() {
            if line.trim().is_empty() {
                continue;
            }
            // Basic open-quote check.
            let quote_count = line.chars().filter(|&c| c == '"').count();
            if quote_count % 2 != 0 {
                return Err(Error::new(format!(
                    "csv: unterminated quoted field in: {line}"
                )));
            }
            records.push(parse_line(line));
        }
        Ok(records)
    }

    /// Serialises `records` into a CSV string.  Fields containing a comma,
    /// double-quote, or newline are quoted; internal double-quotes are
    /// escaped as `""`.
    #[must_use]
    pub fn write(records: &[Vec<String>]) -> String {
        let mut out = String::new();
        for (i, record) in records.iter().enumerate() {
            for (j, field) in record.iter().enumerate() {
                if j > 0 {
                    out.push(',');
                }
                if field.contains(',') || field.contains('"') || field.contains('\n') {
                    out.push('"');
                    out.push_str(&field.replace('"', "\"\""));
                    out.push('"');
                } else {
                    out.push_str(field);
                }
            }
            if i + 1 < records.len() {
                out.push('\n');
            }
        }
        out
    }
}

/// PEM block encoding and decoding.
pub mod pem {
    use crate::errors::Error;

    /// A PEM-encoded block with a type label and decoded bytes.
    #[derive(Debug, Clone, PartialEq)]
    pub struct Block {
        /// The type string, e.g. `"CERTIFICATE"` or `"PRIVATE KEY"`.
        pub block_type: String,
        /// The raw DER-encoded bytes.
        pub bytes: Vec<u8>,
    }

    /// Encodes `block` as a PEM string.
    #[must_use]
    pub fn encode(block: &Block) -> String {
        let b64 = crate::encoding::base64::encode(&block.bytes);
        let mut out = format!("-----BEGIN {}-----\n", block.block_type);
        for chunk in b64.as_bytes().chunks(64) {
            out.push_str(std::str::from_utf8(chunk).unwrap_or(""));
            out.push('\n');
        }
        out.push_str(&format!("-----END {}-----\n", block.block_type));
        out
    }

    /// Decodes all PEM blocks from `input`. Returns an error if any
    /// BEGIN/END pair is mismatched or a base64 payload is invalid.
    pub fn decode_all(input: &str) -> Result<Vec<Block>, Error> {
        let mut blocks = Vec::new();
        let mut remaining = input;

        while let Some(begin_pos) = remaining.find("-----BEGIN ") {
            let rest = &remaining[begin_pos + 11..];
            let Some(end_label) = rest.find("-----") else {
                return Err(Error::new("pem: malformed BEGIN line"));
            };
            let label = rest[..end_label].to_string();
            let after_begin = &rest[end_label + 5..];

            let end_marker = format!("-----END {label}-----");
            let Some(end_pos) = after_begin.find(end_marker.as_str()) else {
                return Err(Error::new(format!("pem: missing END {label}")));
            };
            let b64_text: String = after_begin[..end_pos]
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with("-----"))
                .collect::<Vec<_>>()
                .join("");

            let bytes = crate::encoding::base64::decode(&b64_text)
                .map_err(|e| Error::new(format!("pem: base64 decode: {e}")))?;

            blocks.push(Block {
                block_type: label,
                bytes,
            });

            let consumed = begin_pos + 11 + end_label + 5 + end_pos + end_marker.len();
            if consumed >= remaining.len() {
                break;
            }
            remaining = &remaining[consumed..];
        }

        Ok(blocks)
    }

    /// Decodes the first PEM block from `input`, returning it and any
    /// unparsed remainder.
    pub fn decode(input: &str) -> Result<(Block, &str), Error> {
        let Some(begin_pos) = input.find("-----BEGIN ") else {
            return Err(Error::new("pem: no PEM data found"));
        };
        let rest = &input[begin_pos + 11..];
        let Some(end_label) = rest.find("-----") else {
            return Err(Error::new("pem: malformed BEGIN line"));
        };
        let label = rest[..end_label].to_string();
        let after_begin = &rest[end_label + 5..];
        let end_marker = format!("-----END {label}-----");
        let Some(end_pos) = after_begin.find(end_marker.as_str()) else {
            return Err(Error::new(format!("pem: missing END {label}")));
        };
        let b64_text: String = after_begin[..end_pos]
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("");

        let bytes = crate::encoding::base64::decode(&b64_text)
            .map_err(|e| Error::new(format!("pem: base64 decode: {e}")))?;

        let consumed = begin_pos + 11 + end_label + 5 + end_pos + end_marker.len();
        let rest_input = if consumed < input.len() {
            &input[consumed..]
        } else {
            ""
        };

        Ok((
            Block {
                block_type: label,
                bytes,
            },
            rest_input,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_canonical_vectors() {
        let cases = [
            (b"".as_slice(), ""),
            (b"f".as_slice(), "Zg=="),
            (b"fo".as_slice(), "Zm8="),
            (b"foo".as_slice(), "Zm9v"),
            (b"foob".as_slice(), "Zm9vYg=="),
            (b"fooba".as_slice(), "Zm9vYmE="),
            (b"foobar".as_slice(), "Zm9vYmFy"),
        ];
        for (raw, encoded) in cases {
            assert_eq!(base64::encode(raw), encoded, "encode {raw:?}");
            assert_eq!(base64::decode(encoded).unwrap(), raw);
        }
    }

    #[test]
    fn hex_round_trips_canonical_vectors() {
        assert_eq!(hex::encode(b"abc"), "616263");
        assert_eq!(hex::decode("616263").unwrap(), b"abc");
        assert!(hex::decode("zzz").is_err());
    }

    #[test]
    fn binary_u32_round_trip() {
        let mut buf = [0u8; 4];
        binary::put_u32_be(&mut buf, 0xDEADBEEF);
        assert_eq!(binary::get_u32_be(&buf), 0xDEADBEEF);
        let mut buf = [0u8; 4];
        binary::put_u32_le(&mut buf, 0xCAFEBABE);
        assert_eq!(binary::get_u32_le(&buf), 0xCAFEBABE);
    }
}
