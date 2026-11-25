//! `gos build --size-report`: what each part of a linked executable costs.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;

use object::{Object, ObjectSection, ObjectSymbol, SectionKind, SymbolKind};

/// The names of the functions and data the program's own objects define.
pub(super) type ProgramSymbols = HashSet<Vec<u8>>;

/// The file bytes the symbols of `binary` cover, summed by the part of the
/// build each symbol comes from, largest first. Read from the full symbol
/// table, before a stripped build trims it.
pub(super) fn bytes_by_owner(binary: &[u8], program: &ProgramSymbols) -> Vec<(String, u64)> {
    let Ok(file) = object::File::parse(binary) else {
        return Vec::new();
    };
    let leading_underscore = matches!(file.format(), object::BinaryFormat::MachO);
    let mut by_section: BTreeMap<usize, Vec<(u64, u64, Vec<u8>)>> = BTreeMap::new();
    for symbol in file.symbols() {
        if !symbol.is_definition() || !matches!(symbol.kind(), SymbolKind::Text | SymbolKind::Data)
        {
            continue;
        }
        let (Some(section), Ok(name)) = (symbol.section_index(), symbol.name_bytes()) else {
            continue;
        };
        let name = if leading_underscore {
            name.strip_prefix(b"_").unwrap_or(name)
        } else {
            name
        };
        by_section.entry(section.0).or_default().push((
            symbol.address(),
            symbol.size(),
            name.to_vec(),
        ));
    }
    let mut owners: BTreeMap<String, u64> = BTreeMap::new();
    for (index, mut symbols) in by_section {
        let Ok(section) = file.section_by_index(object::SectionIndex(index)) else {
            continue;
        };
        if section.file_range().is_none() {
            continue;
        }
        let start = section.address();
        let end = start + section.size();
        // A linker can attach a symbol to a section at an address outside it
        // (an end marker past padding); such a symbol owns none of its bytes.
        symbols.retain(|(address, _, _)| (start..end).contains(address));
        symbols.sort_by_key(|(address, _, _)| *address);
        symbols.dedup_by_key(|(address, _, _)| *address);
        for (i, (address, size, name)) in symbols.iter().enumerate() {
            let next = symbols.get(i + 1).map_or(end, |(next, _, _)| *next);
            // A symbol with no recorded size (Mach-O records none) spans up
            // to the next one.
            let span = if *size > 0 {
                (*size).min(next - address)
            } else {
                next - address
            };
            *owners.entry(owner(name, program).into_owned()).or_default() += span;
        }
    }
    let mut owners: Vec<(String, u64)> = owners.into_iter().collect();
    owners.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    owners
}

/// The part of the build the symbol `name` comes from: the program, a Rust
/// crate (named by its legacy-mangled path), the allocator, the unwinder,
/// or the C library and startup code.
fn owner<'a>(name: &'a [u8], program: &ProgramSymbols) -> Cow<'a, str> {
    if program.contains(name) {
        return Cow::Borrowed("program");
    }
    if name.starts_with(b"gos_rt_") {
        return Cow::Borrowed("gossamer_runtime");
    }
    if name.starts_with(b"_ZN9libunwind")
        || name.starts_with(b"__unw_")
        || name.starts_with(b"_Unwind_")
        || name.starts_with(b"__libunwind")
    {
        return Cow::Borrowed("libunwind");
    }
    if let Some(krate) = rust_crate(name) {
        return krate;
    }
    if let Some(krate) = rust_v0_crate(name) {
        return Cow::Owned(krate);
    }
    if name.starts_with(b"_R") {
        return Cow::Borrowed("Rust");
    }
    if name.starts_with(b"mi_") || name.starts_with(b"_mi_") {
        return Cow::Borrowed("mimalloc");
    }
    Cow::Borrowed("C library and startup")
}

/// The crate a legacy-mangled Rust symbol (`_ZN<len><ident>...`) belongs
/// to: its first path segment, or for a trait impl (`<Type as Trait>`) the
/// crate of the implementing type.
fn rust_crate(name: &[u8]) -> Option<Cow<'_, str>> {
    let rest = name.strip_prefix(b"_ZN")?;
    let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
    let len: usize = std::str::from_utf8(&rest[..digits]).ok()?.parse().ok()?;
    let ident = std::str::from_utf8(rest.get(digits..digits + len)?).ok()?;
    let mut path = ident;
    if let Some(inner) = path.strip_prefix("_$LT$") {
        // `<Type as Trait>`: the implementing type's crate when the type is
        // a path, else (a blanket impl over a parameter) the trait's.
        let (self_ty, of_trait) = inner.split_once("$u20$as$u20$").unwrap_or((inner, inner));
        path = if self_ty.contains("..") {
            self_ty
        } else {
            of_trait
        };
        while let Some(stripped) = ["$RF$", "$BP$", "mut$u20$", "dyn$u20$", "$u5b$", "$LP$"]
            .iter()
            .find_map(|prefix| path.strip_prefix(prefix))
        {
            path = stripped;
        }
    }
    let end = path.find(['.', '$']).unwrap_or(path.len());
    let krate = &path[..end];
    if krate.is_empty()
        || !krate
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return None;
    }
    Some(Cow::Owned(krate.to_string()))
}

/// The crate a v0-mangled Rust symbol (`_R...`) belongs to: the crate root
/// its path reaches first, through nested paths (`N`), generic instances
/// (`I`), and inherent or trait impls (`M`, `X`). `None` for a path that
/// starts at a type (`Y`) or anything else this does not read.
fn rust_v0_crate(name: &[u8]) -> Option<String> {
    let mut rest = name.strip_prefix(b"_R")?;
    rest = &rest[rest.iter().take_while(|b| b.is_ascii_digit()).count()..];
    let skip_disambiguator = |rest: &[u8]| -> Option<usize> {
        if rest.first() == Some(&b's') {
            Some(rest.iter().position(|&b| b == b'_')? + 1)
        } else {
            Some(0)
        }
    };
    loop {
        match *rest.first()? {
            b'N' => rest = rest.get(2..)?,
            b'I' => rest = &rest[1..],
            b'M' | b'X' => {
                rest = &rest[1..];
                rest = &rest[skip_disambiguator(rest)?..];
            }
            b'C' => {
                rest = &rest[1..];
                rest = &rest[skip_disambiguator(rest)?..];
                if rest.first() == Some(&b'u') {
                    return None;
                }
                let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
                let len: usize = std::str::from_utf8(&rest[..digits]).ok()?.parse().ok()?;
                rest = &rest[digits..];
                if rest.first() == Some(&b'_') {
                    rest = &rest[1..];
                }
                return std::str::from_utf8(rest.get(..len)?)
                    .ok()
                    .map(str::to_string);
            }
            _ => return None,
        }
    }
}

/// The report `gos build --size-report` prints for the executable `binary`
/// (as published) whose symbols, before any trimming, came to `owners`.
pub(super) fn render(path: &std::path::Path, binary: &[u8], owners: &[(String, u64)]) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "size report: {} ({} bytes)",
        path.display(),
        binary.len()
    );
    if let Ok(file) = object::File::parse(binary) {
        let _ = writeln!(out, "  sections:");
        for section in file.sections() {
            let Ok(name) = section.name() else { continue };
            if name.is_empty() || section.size() == 0 {
                continue;
            }
            let note = match section.kind() {
                SectionKind::UninitializedData | SectionKind::UninitializedTls => " (memory only)",
                _ if section.address() == 0 => " (not loaded)",
                _ => "",
            };
            let _ = writeln!(out, "    {name:<24} {:>12}{note}", section.size());
        }
    }
    let covered: u64 = owners.iter().map(|(_, bytes)| bytes).sum();
    if covered > 0 {
        let _ = writeln!(out, "  code and data by owner:");
        for (owner, bytes) in owners {
            let percent = *bytes as f64 * 100.0 / covered as f64;
            let _ = writeln!(out, "    {owner:<24} {bytes:>12} {percent:>5.1}%");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_symbols_belong_to_their_first_path_segment() {
        let program = ProgramSymbols::new();
        let cases: &[(&[u8], &str)] = &[
            (b"_ZN16gossamer_runtime5c_abi2rc8meta_of17h0123456789abcdefE", "gossamer_runtime"),
            (b"_ZN4core3fmt5write17h0123456789abcdefE", "core"),
            (
                b"_ZN72_$LT$gossamer_runtime..sampler..LazyRing$u20$as$u20$core..fmt..Debug$GT$3fmt17h01E",
                "gossamer_runtime",
            ),
            (b"_ZN44_$LT$$RF$T$u20$as$u20$core..fmt..Display$GT$3fmt17h01E", "core"),
            (b"_ZN9libunwind10CFI_ParserE", "libunwind"),
            (b"_RNvNtCs1234abcd_4core3fmt5write", "core"),
            (b"_RNvMs0_NtCsk2Pk3Wq_3std6threadNtB5_6Thread3new", "std"),
            (b"_RINvNtCs9_5alloc3vec8from_elemhEB4_", "alloc"),
            (b"_RNvXs_NtCsX_9hashbrown3rawINtB4_8RawTableTEENtNtCsY_4core5clone5Clone5clone", "hashbrown"),
            (b"gos_rt_panic", "gossamer_runtime"),
            (b"mi_malloc", "mimalloc"),
            (b"_mi_page_malloc", "mimalloc"),
            (b"memcpy", "C library and startup"),
        ];
        for (name, expected) in cases {
            assert_eq!(
                owner(name, &program),
                *expected,
                "{}",
                String::from_utf8_lossy(name)
            );
        }
    }

    #[test]
    fn a_symbol_outside_its_section_owns_none_of_its_bytes() {
        use object::write::{Object, Symbol, SymbolSection};
        use object::{
            Architecture, BinaryFormat, Endianness, SectionKind, SymbolFlags, SymbolScope,
        };

        let mut obj = Object::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
        let text = obj.add_section(Vec::new(), b".text".to_vec(), SectionKind::Text);
        obj.append_section_data(text, &[0x90; 16], 16);
        for (name, value, size) in [(&b"pick"[..], 0, 8), (&b"past_the_end"[..], 32, 0)] {
            obj.add_symbol(Symbol {
                name: name.to_vec(),
                value,
                size,
                kind: SymbolKind::Text,
                scope: SymbolScope::Linkage,
                weak: false,
                section: SymbolSection::Section(text),
                flags: SymbolFlags::None,
            });
        }
        let binary = obj.write().expect("write object");
        let program: ProgramSymbols = [b"pick".to_vec()].into();
        assert_eq!(
            bytes_by_owner(&binary, &program),
            vec![("program".to_string(), 8)]
        );
    }

    #[test]
    fn the_program_owns_the_symbols_its_objects_define() {
        let program: ProgramSymbols = [b"pick".to_vec(), b"gos_main".to_vec()].into();
        assert_eq!(owner(b"pick", &program), "program");
        assert_eq!(owner(b"gos_main", &program), "program");
        assert_eq!(owner(b"close", &program), "C library and startup");
    }
}
