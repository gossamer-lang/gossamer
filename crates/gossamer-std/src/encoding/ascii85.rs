// Runtime support for `std::encoding::ascii85` - Adobe ASCII85 / btoa.
// The codec is `gossamer_runtime::codec::ascii85`, shared with the compiled
// tiers.

#![forbid(unsafe_code)]

use gossamer_runtime::codec::ascii85;

use crate::errors::Error;

/// Encodes `data` as an ASCII85 string, wrapped in `<~` ... `~>`.
#[must_use]
pub fn encode(data: &[u8]) -> String {
    ascii85::encode(data)
}

/// Decodes an ASCII85 string. The `<~` ... `~>` delimiters are optional but
/// must appear together, and whitespace between digits is skipped.
pub fn decode(s: &str) -> Result<Vec<u8>, Error> {
    ascii85::decode(s).map_err(Error::new)
}
