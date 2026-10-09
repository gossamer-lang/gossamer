//! Text and binary codecs shared by every execution tier.
//!
//! The bytecode VM reaches these through `gossamer-std` and the compiled
//! tiers through the C-ABI shims in `c_abi::encoding`, so each format has one
//! implementation and one set of error messages. Decoders accept exactly what
//! a conforming encoder produces, plus the documented leniencies (whitespace
//! where noted, lowercase and unpadded Base32, optional ASCII85 delimiters),
//! and reject everything else, including spellings that would decode two
//! different texts to the same bytes.

pub mod archive;
pub mod ascii85;
pub mod base32;
pub mod base64;
pub mod compress;
pub mod csv;
pub mod hex;
pub mod html;
pub mod pem;
pub mod percent;
pub mod text;
pub mod varint;
pub mod xml;
