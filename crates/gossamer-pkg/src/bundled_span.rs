use std::path::PathBuf;

/// A byte range of an assembled unit and the file its bytes were read
/// from. Bodies are inlined verbatim - `neutralize_external_mod_decls`
/// blanks a declaration in place rather than resizing it - so a position
/// `p` inside `start..end` sits at `origin_start + (p - start)` in
/// `origin`, which is what maps a diagnostic back to the file the user
/// wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundledSpan {
    /// First byte of the region in the bundled text.
    pub start: u32,
    /// One past the last byte of the region in the bundled text.
    pub end: u32,
    /// File the region's bytes were read from.
    pub origin: PathBuf,
    /// Byte offset of `start` within `origin`.
    pub origin_start: u32,
}
