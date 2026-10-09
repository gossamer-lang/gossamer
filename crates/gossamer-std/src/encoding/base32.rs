// Runtime support for `std::encoding::base32` - RFC 4648 Base32 in the
// standard (A-Z 2-7) and extended-hex (0-9 A-V) alphabets. The codec is
// `gossamer_runtime::codec::base32`, shared with the compiled tiers.

#![forbid(unsafe_code)]

use gossamer_runtime::codec::base32::{self, Alphabet};

/// Encodes `data` using the standard RFC 4648 Base32 alphabet (A-Z 2-7),
/// with `=` padding.
#[must_use]
pub fn encode(data: &[u8]) -> String {
    base32::encode(data, Alphabet::Standard)
}

/// Decodes a standard Base32 string. Lowercase and unpadded text is
/// accepted; anything an encoder would not write is an error.
pub fn decode(s: &str) -> Result<Vec<u8>, String> {
    base32::decode(s, Alphabet::Standard)
}

/// Encodes a UTF-8 string using standard Base32.
#[must_use]
pub fn encode_string(s: &str) -> String {
    encode(s.as_bytes())
}

/// Decodes standard Base32 into a UTF-8 string.
pub fn decode_string(s: &str) -> Result<String, String> {
    let bytes = decode(s)?;
    String::from_utf8(bytes).map_err(|e| format!("base32: {e}"))
}

/// Encodes `data` using the hex (extended) Base32 alphabet (0-9 A-V).
#[must_use]
pub fn encode_hex(data: &[u8]) -> String {
    base32::encode(data, Alphabet::Hex)
}

/// Decodes hex-alphabet Base32.
pub fn decode_hex(s: &str) -> Result<Vec<u8>, String> {
    base32::decode(s, Alphabet::Hex)
}
