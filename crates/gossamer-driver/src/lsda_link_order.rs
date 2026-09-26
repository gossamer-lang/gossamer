//! Ties each function's exception table to its code in a static archive, so
//! a linker that garbage-collects the function drops the table with it.
//!
//! rustc emits one `.gcc_except_table.<fn>` section per function, outside
//! any section group and without `SHF_LINK_ORDER`, because LLVM only marks
//! the table when told the binutils it targets are 2.36 or newer and rustc
//! never says. GNU ld and mold still drop a table only a dead frame refers
//! to; lld keeps every one. Marking the table `SHF_LINK_ORDER` with
//! `sh_link` naming `.text.<fn>` is the layout LLVM itself emits for newer
//! binutils, and it makes every ELF linker discard the table with its code.
//! The edit rewrites two section-header fields in place, so no section moves
//! and no size changes.

/// Section flag: the section's placement and liveness follow `sh_link`.
const SHF_LINK_ORDER: u64 = 0x80;
/// Section type of a COMDAT group.
const SHT_GROUP: u32 = 17;
const ELF_HEADER_LEN: usize = 64;
const SECTION_HEADER_LEN: usize = 64;

/// Marks every function exception table in the ELF64 little-endian objects
/// of the `ar` archive `bytes` as linked to its function's code. Answers how
/// many tables were marked. Members that are not such objects, and tables
/// whose function is absent or sits in a section group, are left as they
/// are.
pub fn link_exception_tables_in_archive(bytes: &mut [u8]) -> usize {
    const MAGIC: &[u8] = b"!<arch>\n";
    const MEMBER_HEADER_LEN: usize = 60;
    if !bytes.starts_with(MAGIC) {
        return 0;
    }
    let mut marked = 0;
    let mut at = MAGIC.len();
    while at + MEMBER_HEADER_LEN <= bytes.len() {
        let header = &bytes[at..at + MEMBER_HEADER_LEN];
        let Some(size) = std::str::from_utf8(&header[48..58])
            .ok()
            .and_then(|text| text.trim().parse::<usize>().ok())
        else {
            break;
        };
        let start = at + MEMBER_HEADER_LEN;
        let Some(end) = start.checked_add(size).filter(|end| *end <= bytes.len()) else {
            break;
        };
        marked += link_exception_tables_in_object(&mut bytes[start..end]);
        // Members start on an even offset.
        at = end + (end & 1);
    }
    marked
}

/// [`link_exception_tables_in_archive`] for one ELF object.
pub fn link_exception_tables_in_object(object: &mut [u8]) -> usize {
    let Some(layout) = SectionLayout::read(object) else {
        return 0;
    };
    let names: Vec<Option<String>> = (0..layout.count)
        .map(|index| layout.name(object, index))
        .collect();
    let mut grouped = vec![false; layout.count];
    for index in 0..layout.count {
        if layout.field_u32(object, index, 4) != Some(SHT_GROUP) {
            continue;
        }
        for member in layout.group_members(object, index) {
            if let Some(slot) = grouped.get_mut(member) {
                *slot = true;
            }
        }
    }
    let mut text_index = std::collections::HashMap::new();
    for (index, name) in names.iter().enumerate() {
        if let Some(function) = name.as_deref().and_then(|n| n.strip_prefix(".text.")) {
            text_index.insert(function.to_string(), index);
        }
    }
    let mut marked = 0;
    for (index, name) in names.iter().enumerate() {
        let Some(function) = name
            .as_deref()
            .and_then(|n| n.strip_prefix(".gcc_except_table."))
        else {
            continue;
        };
        let Some(&code) = text_index.get(function) else {
            continue;
        };
        if grouped[index] || grouped[code] {
            continue;
        }
        let (Some(flags), Ok(link)) = (layout.field_u64(object, index, 8), u32::try_from(code))
        else {
            continue;
        };
        layout.write_u64(object, index, 8, flags | SHF_LINK_ORDER);
        layout.write_u32(object, index, 40, link);
        marked += 1;
    }
    marked
}

/// Where an ELF64 little-endian object keeps its section headers.
struct SectionLayout {
    table: usize,
    count: usize,
    names: usize,
}

impl SectionLayout {
    fn read(object: &[u8]) -> Option<Self> {
        // `\x7fELF`, 64-bit class, little-endian data.
        if object.len() < ELF_HEADER_LEN || &object[..4] != b"\x7fELF" {
            return None;
        }
        if object[4] != 2 || object[5] != 1 {
            return None;
        }
        let table = usize::try_from(read_u64(object, 0x28)?).ok()?;
        if usize::from(read_u16(object, 0x3a)?) != SECTION_HEADER_LEN || table == 0 {
            return None;
        }
        let mut layout = Self {
            table,
            count: usize::from(read_u16(object, 0x3c)?),
            names: usize::from(read_u16(object, 0x3e)?),
        };
        // A count or name index too large for the header field lives in
        // section 0's `sh_size` / `sh_link`.
        if layout.count == 0 {
            layout.count = usize::try_from(layout.field_u64(object, 0, 32)?).ok()?;
        }
        if layout.names == 0xffff {
            layout.names = usize::try_from(layout.field_u32(object, 0, 40)?).ok()?;
        }
        let end = layout
            .count
            .checked_mul(SECTION_HEADER_LEN)?
            .checked_add(table)?;
        (end <= object.len() && layout.names < layout.count).then_some(layout)
    }

    fn at(&self, index: usize, field: usize) -> usize {
        self.table + index * SECTION_HEADER_LEN + field
    }

    fn field_u32(&self, object: &[u8], index: usize, field: usize) -> Option<u32> {
        read_u32(object, self.at(index, field))
    }

    fn field_u64(&self, object: &[u8], index: usize, field: usize) -> Option<u64> {
        read_u64(object, self.at(index, field))
    }

    fn write_u32(&self, object: &mut [u8], index: usize, field: usize, value: u32) {
        let at = self.at(index, field);
        object[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn write_u64(&self, object: &mut [u8], index: usize, field: usize, value: u64) {
        let at = self.at(index, field);
        object[at..at + 8].copy_from_slice(&value.to_le_bytes());
    }

    /// The section's data as `(offset, size)`, when it lies in the object.
    fn data(&self, object: &[u8], index: usize) -> Option<(usize, usize)> {
        let offset = usize::try_from(self.field_u64(object, index, 24)?).ok()?;
        let size = usize::try_from(self.field_u64(object, index, 32)?).ok()?;
        (offset.checked_add(size)? <= object.len()).then_some((offset, size))
    }

    fn name(&self, object: &[u8], index: usize) -> Option<String> {
        let (strings, size) = self.data(object, self.names)?;
        let start = usize::try_from(self.field_u32(object, index, 0)?).ok()?;
        let table = object.get(strings..strings + size)?;
        let rest = table.get(start..)?;
        let len = rest.iter().position(|b| *b == 0)?;
        std::str::from_utf8(&rest[..len]).ok().map(str::to_string)
    }

    /// Section indices a `SHT_GROUP` section lists after its flag word.
    fn group_members(&self, object: &[u8], index: usize) -> Vec<usize> {
        let Some((offset, size)) = self.data(object, index) else {
            return Vec::new();
        };
        (1..size / 4)
            .filter_map(|word| read_u32(object, offset + word * 4))
            .filter_map(|member| usize::try_from(member).ok())
            .collect()
    }
}

fn read_u16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn read_u64(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal ELF64 object: null, `.shstrtab`, `.text.f`,
    /// `.gcc_except_table.f`, and `.gcc_except_table.g` with no code.
    fn object() -> Vec<u8> {
        let names = b"\0.shstrtab\0.text.f\0.gcc_except_table.f\0.gcc_except_table.g\0";
        let name_at = |needle: &str| {
            let text = std::str::from_utf8(names).unwrap();
            u32::try_from(text.find(needle).unwrap()).unwrap()
        };
        let strings_offset = ELF_HEADER_LEN;
        let table = (strings_offset + names.len() + 7) & !7;
        let count = 5usize;
        let mut out = vec![0u8; table + count * SECTION_HEADER_LEN];
        out[..4].copy_from_slice(b"\x7fELF");
        out[4] = 2;
        out[5] = 1;
        out[0x28..0x30].copy_from_slice(&(table as u64).to_le_bytes());
        out[0x3a..0x3c].copy_from_slice(&(SECTION_HEADER_LEN as u16).to_le_bytes());
        out[0x3c..0x3e].copy_from_slice(&(count as u16).to_le_bytes());
        out[0x3e..0x40].copy_from_slice(&1u16.to_le_bytes());
        out[strings_offset..strings_offset + names.len()].copy_from_slice(names);
        let mut header = |index: usize, name: u32, ty: u32, offset: usize, size: usize| {
            let at = table + index * SECTION_HEADER_LEN;
            out[at..at + 4].copy_from_slice(&name.to_le_bytes());
            out[at + 4..at + 8].copy_from_slice(&ty.to_le_bytes());
            out[at + 8..at + 16].copy_from_slice(&2u64.to_le_bytes());
            out[at + 24..at + 32].copy_from_slice(&(offset as u64).to_le_bytes());
            out[at + 32..at + 40].copy_from_slice(&(size as u64).to_le_bytes());
        };
        header(1, name_at(".shstrtab"), 3, strings_offset, names.len());
        header(2, name_at(".text.f"), 1, 0, 0);
        header(3, name_at(".gcc_except_table.f"), 1, 0, 0);
        header(4, name_at(".gcc_except_table.g"), 1, 0, 0);
        out
    }

    #[test]
    fn a_function_table_links_to_its_code() {
        let mut bytes = object();
        assert_eq!(link_exception_tables_in_object(&mut bytes), 1);
        let layout = SectionLayout::read(&bytes).unwrap();
        assert_eq!(layout.field_u64(&bytes, 3, 8), Some(2 | SHF_LINK_ORDER));
        assert_eq!(layout.field_u32(&bytes, 3, 40), Some(2));
        assert_eq!(layout.field_u64(&bytes, 4, 8), Some(2));
        assert_eq!(layout.field_u32(&bytes, 4, 40), Some(0));
    }

    #[test]
    fn archive_members_are_patched_in_place() {
        let member = object();
        let mut archive = b"!<arch>\n".to_vec();
        let header = format!(
            "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
            "f.o/",
            0,
            0,
            0,
            644,
            member.len()
        );
        assert_eq!(header.len(), 60);
        archive.extend_from_slice(header.as_bytes());
        archive.extend_from_slice(&member);
        let before = archive.len();
        assert_eq!(link_exception_tables_in_archive(&mut archive), 1);
        assert_eq!(archive.len(), before);
    }

    #[test]
    fn a_non_elf_member_is_left_alone() {
        let mut bytes = b"not an object".to_vec();
        assert_eq!(link_exception_tables_in_object(&mut bytes), 0);
    }
}
