//! Byte layout of a compiler-typed Gossamer `String`, shared by the runtime
//! that writes it and the back-ends that read it inline.
//!
//! A typed string's content bytes are preceded by a fixed header and followed
//! by a character index, so a scan can read a byte, a length, or a character
//! without calling into the runtime:
//!
//! ```text
//!   [ owner ][ cap:u32 ][ len:u32 ][ tag:u8 ] content... NUL [ index ]
//!   |<- 16 ->|<-  -9  ->|<-  -5  ->|<- -1  ->|^ body           ^ body+cap+1
//! ```
//!
//! Offsets are stated from the body pointer, which is what crosses the C ABI.
//! The index's first `u32` is the character count, or [`INDEX_ASCII`] when
//! every byte is one character - the case where a character index equals its
//! byte offset and the rest of the index is neither written nor read.

/// Offset from the body pointer to the `cap:u32` field.
pub const CAP_OFFSET: i64 = -9;
/// Offset from the body pointer to the `len:u32` field, the content's byte
/// length. Explicit rather than NUL-scanned, so a `String` may hold interior
/// NUL bytes.
pub const LEN_OFFSET: i64 = -5;
/// Offset from the body pointer to the `tag:u8` field.
pub const TAG_OFFSET: i64 = -1;

/// Tag for a growable heap string.
pub const TAG_BUILDER: u8 = 0xAB;
/// Tag for a string whose bytes live in the binary's read-only data.
pub const TAG_STATIC: u8 = 0xA8;
/// Tag for a growable string whose bytes belong to an arena region.
pub const TAG_REGION: u8 = 0xAA;

/// Every tag whose string carries the header this module describes. A pointer
/// whose tag is outside this set is a foreign C string: its length is found by
/// scanning for the NUL and it has no character index.
pub const HEADER_TAGS: [u8; 3] = [TAG_BUILDER, TAG_STATIC, TAG_REGION];

/// Low bits every typed string body address carries, so a candidate pointer is
/// rejected before anything reads in front of it.
pub const BODY_ADDR_TAG: u64 = 5;
/// Mask selecting [`BODY_ADDR_TAG`] from a body address.
pub const BODY_ADDR_MASK: u64 = 7;

/// Character-count sentinel meaning every byte is one character.
pub const INDEX_ASCII: u32 = u32::MAX - 1;
/// Character-count sentinel meaning the index has not been computed.
pub const INDEX_UNSET: u32 = u32::MAX;
/// Characters per block in the non-ASCII index.
pub const INDEX_STRIDE: usize = 32;

/// The two index sentinels are ordered so a reader that has already rejected
/// `INDEX_UNSET` can test the remaining sentinel with one unsigned compare.
const _: () = assert!(INDEX_ASCII < INDEX_UNSET);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_tags_are_distinct() {
        let mut seen = HEADER_TAGS;
        seen.sort_unstable();
        seen.windows(2).for_each(|w| assert_ne!(w[0], w[1]));
    }
}
