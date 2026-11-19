//! The symbol table a stripped ELF executable keeps.
//!
//! A panic trace, a SIGQUIT dump, and a CPU profile name frames through the
//! binary's `.symtab`. Most of that table names the runtime's own Rust
//! functions, whose mangled names are about a fifth of a small program's file.
//! A stripped build keeps only the entries those reports print: the program's
//! functions, the runtime's `gos_rt_*` entry points, and `main`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::size_report::ProgramSymbols;

const SHT_SYMTAB: u32 = 2;
const SHT_STRTAB: u32 = 3;
const SHT_RELA: u32 = 4;
const SHT_NOBITS: u32 = 8;
const SHT_REL: u32 = 9;
const SHT_GROUP: u32 = 17;
const SHT_SYMTAB_SHNDX: u32 = 18;
const SHF_ALLOC: u64 = 0x2;
const STT_FUNC: u8 = 2;
const STB_LOCAL: u8 = 0;

/// Drops from the `.symtab` of the executable at `binary` every symbol a
/// report would not print: all but the functions in `program` (what the
/// program's own objects define), the runtime's `gos_rt_*` entry points, and
/// `main`. A file whose layout the rewrite does not cover (no `.symtab`, or
/// one placed among the loaded segments) keeps its table.
pub(super) fn keep_report_symbols(binary: &Path, program: &ProgramSymbols) -> Result<()> {
    let data = fs::read(binary).with_context(|| format!("reading {}", binary.display()))?;
    let keep =
        |name: &[u8]| program.contains(name) || name.starts_with(b"gos_rt_") || name == b"main";
    if let Some(shrunk) = shrink_symbol_table(&data, &keep) {
        fs::write(binary, shrunk).with_context(|| format!("writing {}", binary.display()))?;
    }
    Ok(())
}

/// The names of the functions and data the object files at `objects`
/// define.
pub(super) fn program_symbols(objects: &[PathBuf]) -> Result<ProgramSymbols> {
    use object::{Object, ObjectSymbol, SymbolKind};

    let mut names = ProgramSymbols::new();
    for path in objects {
        let data = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let file = object::File::parse(&*data)
            .with_context(|| format!("reading the symbols of {}", path.display()))?;
        let leading_underscore = matches!(file.format(), object::BinaryFormat::MachO);
        for symbol in file.symbols() {
            if matches!(symbol.kind(), SymbolKind::Text | SymbolKind::Data)
                && symbol.is_definition()
                && let Ok(name) = symbol.name_bytes()
            {
                let name = if leading_underscore {
                    name.strip_prefix(b"_").unwrap_or(name)
                } else {
                    name
                };
                names.insert(name.to_vec());
            }
        }
    }
    Ok(names)
}

/// Field offsets and widths of one ELF class and byte order.
#[derive(Clone, Copy)]
struct Elf {
    wide: bool,
    big_endian: bool,
}

impl Elf {
    fn read(self, data: &[u8], at: usize, width: usize) -> Option<u64> {
        let bytes = data.get(at..at.checked_add(width)?)?;
        let mut value = 0u64;
        for i in 0..width {
            let byte = if self.big_endian {
                bytes[i]
            } else {
                bytes[width - 1 - i]
            };
            value = (value << 8) | u64::from(byte);
        }
        Some(value)
    }

    fn write(self, data: &mut [u8], at: usize, width: usize, value: u64) {
        for i in 0..width {
            let shift = if self.big_endian { width - 1 - i } else { i } * 8;
            data[at + i] = (value >> shift) as u8;
        }
    }

    /// The width of an address-sized field.
    fn word(self) -> usize {
        if self.wide { 8 } else { 4 }
    }

    fn header_shoff(self) -> usize {
        if self.wide { 0x28 } else { 0x20 }
    }

    fn header_phoff(self) -> usize {
        if self.wide { 0x20 } else { 0x1c }
    }

    /// Offset of `e_phentsize`; `e_phnum`, `e_shentsize`, `e_shnum`, and
    /// `e_shstrndx` follow it as 16-bit fields.
    fn header_phentsize(self) -> usize {
        if self.wide { 0x36 } else { 0x2a }
    }

    fn symbol_size(self) -> usize {
        if self.wide { 24 } else { 16 }
    }

    fn symbol_info(self) -> usize {
        if self.wide { 4 } else { 12 }
    }
}

/// One section header, with the fields the rewrite reads or moves.
#[derive(Clone, Copy)]
struct Section {
    kind: u32,
    flags: u64,
    offset: u64,
    size: u64,
    link: u32,
    info: u32,
    align: u64,
}

impl Section {
    fn parse(elf: Elf, data: &[u8], at: usize) -> Option<Self> {
        let w = elf.word();
        Some(Self {
            kind: elf.read(data, at + 4, 4)? as u32,
            flags: elf.read(data, at + 8, w)?,
            offset: elf.read(data, at + 8 + 2 * w, w)?,
            size: elf.read(data, at + 8 + 3 * w, w)?,
            link: elf.read(data, at + 8 + 4 * w, 4)? as u32,
            info: elf.read(data, at + 12 + 4 * w, 4)? as u32,
            align: elf.read(data, at + 16 + 4 * w, w)?,
        })
    }

    /// Writes back the fields the rewrite changes: offset, size, and info.
    fn store(self, elf: Elf, data: &mut [u8], at: usize) {
        let w = elf.word();
        elf.write(data, at + 8 + 2 * w, w, self.offset);
        elf.write(data, at + 8 + 3 * w, w, self.size);
        elf.write(data, at + 12 + 4 * w, 4, u64::from(self.info));
    }

    fn occupies_file(self) -> bool {
        self.kind != 0 && self.kind != SHT_NOBITS && self.size > 0
    }

    fn range(self) -> Option<std::ops::Range<usize>> {
        let start = usize::try_from(self.offset).ok()?;
        Some(start..start.checked_add(usize::try_from(self.size).ok()?)?)
    }
}

/// The ELF header fields the rewrite reads.
struct Header {
    shoff: usize,
    phoff: usize,
    phentsize: usize,
    phnum: usize,
    shentsize: usize,
    shnum: usize,
    shstrndx: usize,
}

impl Header {
    /// The class, byte order, and table positions of the ELF file `data`,
    /// or `None` for another file or one with no section headers.
    fn parse(data: &[u8]) -> Option<(Elf, Self)> {
        if data.get(..4)? != b"\x7fELF" {
            return None;
        }
        let elf = Elf {
            wide: match *data.get(4)? {
                1 => false,
                2 => true,
                _ => return None,
            },
            big_endian: match *data.get(5)? {
                1 => false,
                2 => true,
                _ => return None,
            },
        };
        let fields = elf.header_phentsize();
        let header = Self {
            shoff: usize::try_from(elf.read(data, elf.header_shoff(), elf.word())?).ok()?,
            phoff: usize::try_from(elf.read(data, elf.header_phoff(), elf.word())?).ok()?,
            phentsize: elf.read(data, fields, 2)? as usize,
            phnum: elf.read(data, fields + 2, 2)? as usize,
            shentsize: elf.read(data, fields + 4, 2)? as usize,
            shnum: elf.read(data, fields + 6, 2)? as usize,
            shstrndx: elf.read(data, fields + 8, 2)? as usize,
        };
        (header.shnum > 0 && header.shoff > 0).then_some((elf, header))
    }
}

/// The end of the last file byte a loaded section or a segment covers, the
/// program headers included.
fn loaded_end(elf: Elf, data: &[u8], header: &Header, sections: &[Section]) -> Option<usize> {
    let Header {
        phoff,
        phentsize,
        phnum,
        ..
    } = *header;
    sections
        .iter()
        .filter(|s| s.flags & SHF_ALLOC != 0 && s.occupies_file())
        .map(|s| s.range().map(|r| r.end))
        .chain((0..phnum).map(|i| {
            let at = phoff + i * phentsize;
            let w = elf.word();
            let (offset, filesz) = if elf.wide {
                (elf.read(data, at + 8, w)?, elf.read(data, at + 0x20, w)?)
            } else {
                (elf.read(data, at + 4, w)?, elf.read(data, at + 0x10, w)?)
            };
            usize::try_from(offset.checked_add(filesz)?).ok()
        }))
        .try_fold(phoff + phnum * phentsize, |end, next| Some(end.max(next?)))
}

/// `data` with its `.symtab` holding only the function symbols `keep`
/// accepts, and its string table only their names. The sections that are not
/// loaded (the symbol and string tables among them) sit after every loaded
/// byte in the file, so they are laid out again from the first of them and
/// the loaded image is untouched. `None` when `data` has no `.symtab` or
/// another layout.
fn shrink_symbol_table(data: &[u8], keep: &dyn Fn(&[u8]) -> bool) -> Option<Vec<u8>> {
    let (elf, header) = Header::parse(data)?;
    let Header {
        shoff,
        shentsize,
        shnum,
        shstrndx,
        ..
    } = header;
    let sections: Vec<Section> = (0..shnum)
        .map(|i| Section::parse(elf, data, shoff + i * shentsize))
        .collect::<Option<_>>()?;

    let symtab_index = sections.iter().position(|s| s.kind == SHT_SYMTAB)?;
    let symtab = sections[symtab_index];
    let strtab_index = symtab.link as usize;
    if strtab_index == shstrndx || sections.get(strtab_index)?.kind != SHT_STRTAB {
        return None;
    }
    // A section that names symbols by index, or names its own strings in
    // the symbol strings, would read the wrong entries once they move.
    let depends_on_tables = sections.iter().enumerate().any(|(i, s)| {
        i != symtab_index
            && ((s.link as usize == symtab_index
                && matches!(s.kind, SHT_REL | SHT_RELA | SHT_GROUP | SHT_SYMTAB_SHNDX))
                || s.link as usize == strtab_index)
    });
    if depends_on_tables {
        return None;
    }

    let tail_start = sections
        .iter()
        .filter(|s| s.flags & SHF_ALLOC == 0 && s.occupies_file())
        .map(|s| s.offset)
        .min()?;
    let tail_start = usize::try_from(tail_start).ok()?;
    if loaded_end(elf, data, &header, &sections)? > tail_start || shoff < tail_start {
        return None;
    }

    let (symbols, strings, locals) =
        kept_symbols(elf, data, symtab, &sections[strtab_index], keep)?;

    let mut tail: Vec<(usize, Section)> = sections
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, s)| s.flags & SHF_ALLOC == 0 && s.occupies_file())
        .collect();
    tail.sort_by_key(|(_, s)| s.offset);
    let mut out = data[..tail_start].to_vec();
    let mut moved = sections.clone();
    for (index, section) in tail {
        let align = usize::try_from(section.align.max(1)).ok()?;
        out.resize(out.len().next_multiple_of(align), 0);
        let offset = out.len();
        if index == symtab_index {
            out.extend_from_slice(&symbols);
            moved[index].info = locals;
        } else if index == strtab_index {
            out.extend_from_slice(&strings);
        } else {
            out.extend_from_slice(data.get(section.range()?)?);
        }
        moved[index].offset = offset as u64;
        moved[index].size = (out.len() - offset) as u64;
    }
    out.resize(out.len().next_multiple_of(elf.word()), 0);
    let new_shoff = out.len();
    out.extend_from_slice(data.get(shoff..shoff + shnum * shentsize)?);
    for (i, section) in moved.iter().enumerate() {
        section.store(elf, &mut out, new_shoff + i * shentsize);
    }
    elf.write(&mut out, elf.header_shoff(), elf.word(), new_shoff as u64);
    Some(out)
}

/// The kept entries of `symtab`, in their original order behind the null
/// entry, their string table, and how many of them (the null entry
/// included) are local, which is the table's new `sh_info`.
fn kept_symbols(
    elf: Elf,
    data: &[u8],
    symtab: Section,
    strtab: &Section,
    keep: &dyn Fn(&[u8]) -> bool,
) -> Option<(Vec<u8>, Vec<u8>, u32)> {
    let entries = data.get(symtab.range()?)?;
    let names = data.get(strtab.range()?)?;
    let size = elf.symbol_size();
    let mut symbols = entries.get(..size)?.to_vec();
    let mut strings = vec![0u8];
    let mut placed: std::collections::HashMap<&[u8], u32> = std::collections::HashMap::new();
    let mut locals = 1u32;
    for entry in entries.chunks_exact(size).skip(1) {
        let info = entry[elf.symbol_info()];
        if info & 0xf != STT_FUNC {
            continue;
        }
        let name_at = usize::try_from(elf.read(entry, 0, 4)?).ok()?;
        let tail = names.get(name_at..)?;
        let name = &tail[..tail.iter().position(|&b| b == 0)?];
        if name.is_empty() || !keep(name) {
            continue;
        }
        let offset = *placed.entry(name).or_insert_with(|| {
            let offset = strings.len() as u32;
            strings.extend_from_slice(name);
            strings.push(0);
            offset
        });
        let start = symbols.len();
        symbols.extend_from_slice(entry);
        elf.write(&mut symbols, start, 4, u64::from(offset));
        if info >> 4 == STB_LOCAL {
            locals += 1;
        }
    }
    Some((symbols, strings, locals))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This test binary is an ELF executable with a full `.symtab` and,
    /// as a debug build, many unloaded sections around it.
    #[test]
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    #[cfg_attr(
        not(debug_assertions),
        ignore = "the release profile strips the symbol table this test reads"
    )]
    fn shrinking_keeps_the_loaded_image_and_only_the_kept_functions() {
        use object::{Object, ObjectSection, ObjectSymbol, ObjectSymbolTable, SymbolKind};

        let exe = std::env::current_exe().expect("test executable");
        let data = fs::read(&exe).expect("read test executable");
        let original = object::File::parse(&*data).expect("parse original");
        let wanted: Vec<Vec<u8>> = original
            .symbols()
            .filter(|s| s.kind() == SymbolKind::Text && s.is_definition())
            .filter_map(|s| s.name_bytes().ok().map(<[u8]>::to_vec))
            .filter(|name| name.starts_with(b"_ZN") && name.windows(7).any(|w| w == b"symbols"))
            .collect();
        assert!(
            !wanted.is_empty(),
            "this module's functions are in the table"
        );

        let shrunk = shrink_symbol_table(&data, &|name| wanted.iter().any(|w| w == name))
            .expect("a linked ELF executable has the layout the rewrite covers");
        assert!(shrunk.len() < data.len());

        let file = object::File::parse(&*shrunk).expect("parse shrunk");
        let names: Vec<&[u8]> = file
            .symbol_table()
            .expect("a symbol table")
            .symbols()
            .filter_map(|s| s.name_bytes().ok())
            .filter(|n| !n.is_empty())
            .collect();
        assert!(!names.is_empty());
        assert!(
            names.iter().all(|n| wanted.iter().any(|w| w == n)),
            "{names:?}"
        );
        for section in original.sections() {
            let Some((offset, size)) = section.file_range() else {
                continue;
            };
            if section.address() == 0 {
                continue;
            }
            let range = offset as usize..(offset + size) as usize;
            assert_eq!(
                data[range.clone()],
                shrunk[range],
                "loaded section {:?} moved",
                section.name()
            );
        }
        for kept in &names {
            let before = original
                .symbols()
                .find(|s| s.name_bytes().ok() == Some(kept))
                .expect("kept symbol was in the original");
            let after = file
                .symbols()
                .find(|s| s.name_bytes().ok() == Some(kept))
                .expect("kept symbol");
            assert_eq!(before.address(), after.address());
            assert_eq!(before.size(), after.size());
        }
    }

    #[test]
    fn data_that_is_not_elf_is_left_alone() {
        assert!(shrink_symbol_table(b"MZ\x90\x00", &|_| true).is_none());
        assert!(shrink_symbol_table(b"", &|_| true).is_none());
    }
}
