//! Registry of source files and byte-offset to line/column resolution.

use crate::span::{FileId, LineCol, Span};

/// A region of an assembled file and the already-registered file its
/// bytes were read from. Bytes are embedded verbatim, so a position `p`
/// in `start..end` sits at `origin_start + (p - start)` in `origin`.
#[derive(Debug, Clone, Copy)]
pub struct OriginSpan {
    /// First byte of the region in the assembled file.
    pub start: u32,
    /// One past the last byte of the region in the assembled file.
    pub end: u32,
    /// File the region's bytes were read from.
    pub origin: FileId,
    /// Byte offset of `start` within `origin`.
    pub origin_start: u32,
}

/// A single file registered with a `SourceMap`.
#[derive(Debug)]
struct SourceFile {
    name: String,
    source: String,
    line_starts: Vec<u32>,
    /// Provenance of an assembled file's regions, innermost last so the
    /// last match wins for nested embeddings.
    origins: Vec<OriginSpan>,
}

impl SourceFile {
    /// Builds a new source file record, indexing line start offsets.
    fn new(name: String, mut source: String) -> Self {
        // Strip a leading UTF-8 BOM (U+FEFF, 3 bytes) so byte offsets
        // match the BOM-less basis the parser uses (see Parser::new,
        // which strips the same prefix before lexing). Without this,
        // every span on a BOM-prefixed file is shifted 3 bytes and
        // diagnostics point one position off.
        if source.starts_with('\u{feff}') {
            source.drain(..'\u{feff}'.len_utf8());
        }
        let mut line_starts = vec![0u32];
        for (index, byte) in source.bytes().enumerate() {
            if byte == b'\n' {
                let next_line_start = u32::try_from(index + 1).expect("source file exceeds 4 GiB");
                line_starts.push(next_line_start);
            }
        }
        Self {
            name,
            source,
            line_starts,
            origins: Vec::new(),
        }
    }

    /// Returns the one-based line and column for `offset` in this file.
    fn line_col(&self, offset: u32) -> LineCol {
        let line_index = match self.line_starts.binary_search(&offset) {
            Ok(exact) => exact,
            Err(after) => after.saturating_sub(1),
        };
        let line_start = self.line_starts[line_index];
        let column_bytes = &self.source.as_bytes()[line_start as usize..offset as usize];
        let column_chars =
            std::str::from_utf8(column_bytes).map_or(column_bytes.len(), |s| s.chars().count());
        LineCol {
            line: u32::try_from(line_index + 1).unwrap_or(u32::MAX),
            column: u32::try_from(column_chars + 1).unwrap_or(u32::MAX),
        }
    }
}

/// Registry of source files. Gives every file a stable `FileId` and
/// resolves `Span`s back to line/column positions.
#[derive(Debug, Default)]
pub struct SourceMap {
    files: Vec<SourceFile>,
}

impl SourceMap {
    /// Returns an empty source map.
    #[must_use]
    pub const fn new() -> Self {
        Self { files: Vec::new() }
    }

    /// Registers a new file and returns its `FileId`.
    pub fn add_file(&mut self, name: impl Into<String>, source: impl Into<String>) -> FileId {
        let file_id = FileId(u32::try_from(self.files.len()).expect("too many source files"));
        self.files.push(SourceFile::new(name.into(), source.into()));
        file_id
    }

    /// Returns the display name registered for `file`.
    #[must_use]
    pub fn file_name(&self, file: FileId) -> &str {
        &self.files[file.0 as usize].name
    }

    /// Returns the full source text of `file`.
    #[must_use]
    pub fn source(&self, file: FileId) -> &str {
        &self.files[file.0 as usize].source
    }

    /// Consumes the map and returns the owned source text for `file`.
    ///
    /// This is useful for frontend pipelines that temporarily need span
    /// lookup but must return the original source on an early exit without
    /// keeping or creating a second complete `String`.
    #[must_use]
    pub fn into_source(mut self, file: FileId) -> String {
        self.files.swap_remove(file.0 as usize).source
    }

    /// Returns the source slice covered by `span`.
    #[must_use]
    pub fn slice(&self, span: Span) -> &str {
        let source = self.source(span.file);
        &source[span.start as usize..span.end as usize]
    }

    /// Returns the one-based line and column of `offset` in `file`.
    #[must_use]
    pub fn line_col(&self, file: FileId, offset: u32) -> LineCol {
        self.files[file.0 as usize].line_col(offset)
    }

    /// Records where the regions of an assembled `file` were read from.
    /// Push order is outermost first: a later span covering the same
    /// position describes a more deeply embedded file and wins.
    pub fn set_origins(&mut self, file: FileId, origins: Vec<OriginSpan>) {
        self.files[file.0 as usize].origins = origins;
    }

    /// Records one more region of an assembled `file`, innermost of those
    /// recorded so far.
    pub fn add_origin(&mut self, file: FileId, origin: OriginSpan) {
        self.files[file.0 as usize].origins.push(origin);
    }

    /// The recorded provenance of `file`'s regions, outermost first.
    #[must_use]
    pub fn origins(&self, file: FileId) -> &[OriginSpan] {
        &self.files[file.0 as usize].origins
    }

    /// Resolves a position in an assembled file to the file its bytes
    /// were read from and the matching position there. Returns `(file,
    /// offset)` unchanged for a file with no recorded provenance, and for
    /// a position no recorded region covers.
    #[must_use]
    pub fn origin_of(&self, file: FileId, offset: u32) -> (FileId, u32) {
        let (mut file, mut offset) = (file, offset);
        // A nested unit can itself be assembled, so keep resolving until
        // the position lands in a file read straight from disk. Each step
        // moves to a different file, so the file count bounds the walk.
        for _ in 0..self.files.len() {
            let Some(span) = self.files[file.0 as usize]
                .origins
                .iter()
                .rev()
                .find(|span| offset >= span.start && offset < span.end)
            else {
                return (file, offset);
            };
            if span.origin == file {
                return (file, offset);
            }
            offset = span.origin_start + (offset - span.start);
            file = span.origin;
        }
        (file, offset)
    }
}

#[cfg(test)]
mod source_map_tests {
    use super::*;

    #[test]
    fn a_position_in_an_assembled_region_resolves_to_its_origin_file() {
        let mut map = SourceMap::new();
        let helper = map.add_file("helper.gos", "pub fn h() { }\n".to_string());
        let unit = map.add_file(
            "main.gos",
            "fn main() { }\npub mod helper {\npub fn h() { }\n}\n".to_string(),
        );
        // The inlined body starts after `fn main() { }\npub mod helper {\n`.
        let body_start = 31u32;
        map.set_origins(
            unit,
            vec![OriginSpan {
                start: body_start,
                end: body_start + 15,
                origin: helper,
                origin_start: 0,
            }],
        );
        let (origin, offset) = map.origin_of(unit, body_start + 7);
        assert_eq!(map.file_name(origin), "helper.gos");
        assert_eq!(offset, 7);
        assert_eq!(map.line_col(origin, offset).line, 1);
    }

    #[test]
    fn a_position_outside_every_recorded_region_stays_on_the_unit() {
        let mut map = SourceMap::new();
        let helper = map.add_file("helper.gos", "pub fn h() { }\n".to_string());
        let unit = map.add_file("main.gos", "fn main() { }\n".to_string());
        map.set_origins(
            unit,
            vec![OriginSpan {
                start: 100,
                end: 115,
                origin: helper,
                origin_start: 0,
            }],
        );
        let (origin, offset) = map.origin_of(unit, 3);
        assert_eq!(origin, unit);
        assert_eq!(offset, 3);
    }
}
