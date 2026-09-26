//! What a linked binary carries: a release build keeps no exception table of
//! a function the link dropped, and a `-g` build's line table names the
//! Gossamer source it was built from.
#![cfg(target_os = "linux")]

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

fn gos_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gos"))
}

/// Section flag: the section's bytes are an `Elf64_Chdr` and a zlib stream.
const SHF_COMPRESSED: u64 = 0x800;

/// The named sections of an ELF64 little-endian file: name, flags, bytes.
fn sections(file: &[u8]) -> Vec<(String, u64, Vec<u8>)> {
    let u16_at = |at: usize| u16::from_le_bytes(file[at..at + 2].try_into().unwrap());
    let u32_at = |at: usize| u32::from_le_bytes(file[at..at + 4].try_into().unwrap());
    let u64_at = |at: usize| u64::from_le_bytes(file[at..at + 8].try_into().unwrap());
    assert_eq!(&file[..4], b"\x7fELF", "not an ELF file");
    let table = u64_at(0x28) as usize;
    let count = u16_at(0x3c) as usize;
    let names = u16_at(0x3e) as usize;
    let header = |index: usize| table + index * 64;
    let data = |index: usize| {
        let at = u64_at(header(index) + 24) as usize;
        let size = u64_at(header(index) + 32) as usize;
        // `SHT_NOBITS` occupies no file bytes.
        if u32_at(header(index) + 4) == 8 {
            Vec::new()
        } else {
            file[at..at + size].to_vec()
        }
    };
    let strings = data(names);
    (0..count)
        .map(|index| {
            let start = u32_at(header(index)) as usize;
            let end = strings[start..].iter().position(|b| *b == 0).unwrap() + start;
            let name = String::from_utf8_lossy(&strings[start..end]).into_owned();
            (name, u64_at(header(index) + 8), data(index))
        })
        .collect()
}

fn section(file: &[u8], wanted: &str) -> Option<Vec<u8>> {
    let (_, flags, bytes) = sections(file)
        .into_iter()
        .find(|(name, ..)| name == wanted)?;
    if flags & SHF_COMPRESSED == 0 {
        return Some(bytes);
    }
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(&bytes[24..])
        .read_to_end(&mut out)
        .expect("inflate a compressed section");
    Some(out)
}

fn build(dir: &Path, name: &str, source: &str, flags: &[&str]) -> Vec<u8> {
    std::fs::write(dir.join(format!("{name}.gos")), source).expect("write program");
    let out_dir = format!("out-{name}");
    let file = format!("{name}.gos");
    let mut args = vec!["build"];
    args.extend_from_slice(flags);
    args.extend([file.as_str(), "--out-dir", out_dir.as_str()]);
    let output = Command::new(gos_bin())
        .current_dir(dir)
        .env("GOSSAMER_CACHE_DIR", dir.join("cache"))
        .args(&args)
        .output()
        .expect("spawn gos build");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::read(dir.join(out_dir).join(name)).expect("read the artifact")
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gos-layout-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

#[test]
fn a_release_build_drops_the_exception_tables_of_dropped_functions() {
    let dir = scratch("lsda");
    let binary = build(
        &dir,
        "hello",
        "fn main() {\n    println(\"hello\")\n}\n",
        &["--release"],
    );
    let tables = section(&binary, ".gcc_except_table").map_or(0, |bytes| bytes.len());
    // The runtime carries thousands of tables; a hello world reaches a few
    // dozen functions that have one.
    assert!(tables < 128 * 1024, ".gcc_except_table is {tables} bytes");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_debug_info_build_has_a_line_table_for_the_gossamer_source() {
    let dir = scratch("dwarf");
    let source =
        "fn inner() -> i64 {\n    40 + 2\n}\n\nfn main() {\n    println(\"{}\", inner())\n}\n";
    for flags in [&["-g"][..], &["-g", "--release"][..]] {
        let binary = build(&dir, "lines", source, flags);
        let lines = section(&binary, ".debug_line").expect("a .debug_line section");
        assert!(
            lines
                .windows(b"lines.gos\0".len())
                .any(|w| w == b"lines.gos\0"),
            "{flags:?}: the line table names no Gossamer file"
        );
        let _ = std::fs::remove_dir_all(dir.join("out-lines"));
    }
    let _ = std::fs::remove_dir_all(&dir);
}
