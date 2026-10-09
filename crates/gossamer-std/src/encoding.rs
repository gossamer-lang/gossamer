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
    //! RFC 4648 base64 with the standard alphabet; the codec is
    //! `gossamer_runtime::codec::base64`, shared with the compiled tiers.

    use gossamer_runtime::codec::base64;

    use crate::errors::Error;

    /// Encodes `input` to a base64 string (with `=` padding).
    #[must_use]
    pub fn encode(input: &[u8]) -> String {
        base64::encode(input)
    }

    /// Decodes a base64 string, tolerating whitespace between characters.
    /// The input is whole groups of four; `=` pads only the last group,
    /// as `xx==` or `xxx=`, and the bits it leaves over are zero.
    pub fn decode(input: &str) -> Result<Vec<u8>, Error> {
        base64::decode(input).map_err(Error::new)
    }
}

pub mod hex {
    //! Lowercase hex encoding; the codec is `gossamer_runtime::codec::hex`,
    //! shared with the compiled tiers.

    use gossamer_runtime::codec::hex;

    use crate::errors::Error;

    /// Encodes `input` as lowercase hex.
    #[must_use]
    pub fn encode(input: &[u8]) -> String {
        hex::encode(input)
    }

    /// Decodes hex of either case, skipping whitespace between digits and
    /// rejecting non-hex characters and an odd digit count.
    pub fn decode(input: &str) -> Result<Vec<u8>, Error> {
        hex::decode(input).map_err(Error::new)
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
        let bytes = gossamer_runtime::codec::varint::encode_unsigned(x);
        buf[..bytes.len()].copy_from_slice(&bytes);
        bytes.len()
    }

    /// Encodes `x` as a signed varint using zigzag encoding.
    /// Returns the number of bytes written.
    pub fn put_varint(buf: &mut [u8], x: i64) -> usize {
        let bytes = gossamer_runtime::codec::varint::encode_signed(x);
        buf[..bytes.len()].copy_from_slice(&bytes);
        bytes.len()
    }

    /// Decodes an unsigned varint from `buf`.
    /// Returns `(value, bytes_consumed)` or an error.
    pub fn uvarint(buf: &[u8]) -> Result<(u64, usize), Error> {
        gossamer_runtime::codec::varint::decode_unsigned(buf).map_err(Error::new)
    }

    /// Decodes a signed varint (zigzag) from `buf`.
    /// Returns `(value, bytes_consumed)` or an error.
    pub fn varint(buf: &[u8]) -> Result<(i64, usize), Error> {
        gossamer_runtime::codec::varint::decode_signed(buf).map_err(Error::new)
    }
}

/// CSV reading and writing, over the `gossamer_runtime::codec::csv` codec
/// the compiled tiers share.
pub mod csv {
    use gossamer_runtime::codec::csv;

    use crate::errors::Error;

    /// Parses a single CSV-formatted line, respecting double-quoted fields
    /// and escaped quotes (`""`).
    #[must_use]
    pub fn parse_line(line: &str) -> Vec<String> {
        csv::parse_line(line)
    }

    /// Parses all records from a CSV string. Each record is a `Vec<String>`;
    /// a quoted field may span lines, and blank lines are skipped. Returns
    /// an error if a quoted field is never closed.
    pub fn read(input: &str) -> Result<Vec<Vec<String>>, Error> {
        csv::read(input).map_err(Error::new)
    }

    /// Serialises `records` into a CSV string. Fields containing a comma,
    /// double-quote, or line break are quoted; internal double-quotes are
    /// escaped as `""`.
    #[must_use]
    pub fn write(records: &[Vec<String>]) -> String {
        csv::write(records)
    }
}

/// PEM block encoding and decoding.
pub mod pem {
    use gossamer_runtime::codec::pem;

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
        pem::encode(&block.block_type, &block.bytes)
    }

    /// Decodes all PEM blocks from `input`. Returns an error if any
    /// BEGIN/END pair is mismatched or a base64 payload is invalid.
    pub fn decode_all(input: &str) -> Result<Vec<Block>, Error> {
        let blocks = pem::decode_all(input).map_err(Error::new)?;
        Ok(blocks.into_iter().map(Block::from).collect())
    }

    /// Decodes the first PEM block from `input`, returning it and any
    /// unparsed remainder.
    pub fn decode(input: &str) -> Result<(Block, &str), Error> {
        let (block, rest) = pem::decode(input).map_err(Error::new)?;
        Ok((Block::from(block), rest))
    }

    impl From<pem::Block> for Block {
        fn from(block: pem::Block) -> Self {
            Self {
                block_type: block.label,
                bytes: block.bytes,
            }
        }
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
