/// Unit name and line-start offsets registered by [`set_source_positions`].
static SOURCE_POSITIONS: std::sync::RwLock<Option<SourcePositions>> = std::sync::RwLock::new(None);

/// Where each line of a compiled unit begins, and which file each region of
/// the unit was read from.
///
/// A project is compiled as one assembled unit, but a panic report names the
/// file a function was written in and the line within that file. Resolving
/// through the region's own file also keeps a module's generated code - and
/// so its cached object - independent of edits to the files assembled around
/// it.
#[derive(Debug, Clone, Default)]
pub struct SourcePositions {
    unit: String,
    unit_text: SourceText,
    files: Vec<(String, SourceText)>,
    regions: Vec<SourceRegion>,
}

/// A file's text with the byte offset each of its lines begins at.
#[derive(Debug, Clone, Default)]
struct SourceText {
    text: String,
    line_starts: Vec<u32>,
}

impl SourceText {
    fn new(text: &str) -> Self {
        Self {
            text: text.to_string(),
            line_starts: line_starts(text),
        }
    }

    /// The one-based line and column of `offset`, the column counted in
    /// characters as the source map counts it.
    fn line_column(&self, offset: u32) -> (u32, u32) {
        // `partition_point` gives the count of line starts at or before the
        // offset, which is exactly the one-based line number.
        let line = self
            .line_starts
            .partition_point(|start| *start <= offset)
            .max(1);
        let start = self.line_starts[line - 1] as usize;
        let end = (offset as usize).clamp(start, self.text.len());
        let column = self
            .text
            .get(start..end)
            .map_or(end - start, |prefix| prefix.chars().count());
        (
            u32::try_from(line).unwrap_or(u32::MAX),
            u32::try_from(column + 1).unwrap_or(u32::MAX),
        )
    }
}

#[derive(Debug, Clone, Copy)]
struct SourceRegion {
    start: u32,
    end: u32,
    origin_start: u32,
    file: usize,
}

/// The byte offset each line of `text` begins at.
fn line_starts(text: &str) -> Vec<u32> {
    std::iter::once(0)
        .chain(
            text.bytes()
                .enumerate()
                .filter(|&(_, byte)| byte == b'\n')
                .map(|(offset, _)| u32::try_from(offset + 1).unwrap_or(u32::MAX)),
        )
        .collect()
}

impl SourcePositions {
    /// A table for the unit named `unit`, whose assembled text is `source`.
    #[must_use]
    pub fn new(unit: impl Into<String>, source: &str) -> Self {
        Self {
            unit: unit.into(),
            unit_text: SourceText::new(source),
            files: Vec::new(),
            regions: Vec::new(),
        }
    }

    /// Records that the unit's bytes `start..end` were read from `file`,
    /// whose text is `file_source`, beginning at `origin_start`. A later
    /// region covering the same position wins, as a more deeply embedded
    /// file does in the source map.
    pub fn add_region(
        &mut self,
        start: u32,
        end: u32,
        origin_start: u32,
        file: &str,
        file_source: &str,
    ) {
        let index = if let Some(index) = self.files.iter().position(|(name, _)| name == file) {
            index
        } else {
            self.files
                .push((file.to_string(), SourceText::new(file_source)));
            self.files.len() - 1
        };
        self.regions.push(SourceRegion {
            start,
            end,
            origin_start,
            file: index,
        });
    }

    /// The file and the one-based line and column a unit offset was written
    /// at.
    fn position(&self, offset: u32) -> (String, u32, u32) {
        if let Some(region) = self
            .regions
            .iter()
            .rev()
            .find(|region| offset >= region.start && offset < region.end)
        {
            let (name, text) = &self.files[region.file];
            let (line, column) = text.line_column(region.origin_start + (offset - region.start));
            (name.clone(), line, column)
        } else {
            let (line, column) = self.unit_text.line_column(offset);
            (self.unit.clone(), line, column)
        }
    }
}

/// Registers the position table codegen resolves MIR spans through. The
/// source map does not survive the frontend, so the driver hands over this
/// compact form before codegen runs.
pub fn set_source_positions(positions: SourcePositions) {
    let mut slot = SOURCE_POSITIONS
        .write()
        .expect("source-position table lock poisoned");
    *slot = Some(positions);
}

/// The file name and the one-based line and column for `offset`, or `None`
/// when no table has been registered for this build.
pub fn source_position(offset: u32) -> Option<(String, u32, u32)> {
    let slot = SOURCE_POSITIONS
        .read()
        .expect("source-position table lock poisoned");
    Some(slot.as_ref()?.position(offset))
}
