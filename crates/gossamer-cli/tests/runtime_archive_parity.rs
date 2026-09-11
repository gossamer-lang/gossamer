//! The host runtime archive and the static-musl one are one toolchain: an
//! install publishes both into `<prefix>/lib`, `gos build --release` links the
//! musl archive and every other link takes the host one. A `gos_rt_*` symbol
//! that only one of them defines is an undefined symbol at link time in a
//! program that never names it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The `target/<profile>` directory the CLI build script publishes both
/// runtime archives into.
fn artifact_dir() -> PathBuf {
    let gos = PathBuf::from(std::env::var("CARGO_BIN_EXE_gos").expect("CARGO_BIN_EXE_gos"));
    gos.parent()
        .expect("target profile directory")
        .to_path_buf()
}

/// Reads a fixed-width ASCII field out of an `ar` member header.
fn header_field(header: &[u8], start: usize, end: usize) -> String {
    String::from_utf8_lossy(&header[start..end])
        .trim()
        .to_string()
}

/// Every externally visible symbol the System V `ar` index names, which is
/// every symbol a link against the archive can resolve.
///
/// The index is the archive's first member: a big-endian count, that many
/// big-endian member offsets, then one NUL-terminated name per offset. The
/// `/SYM64/` spelling is the same layout at 64-bit width.
fn archive_symbols(path: &Path) -> Result<Vec<String>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    if bytes.get(..8) != Some(b"!<arch>\n".as_slice()) {
        return Err(format!("{} is not a System V archive", path.display()));
    }
    let header = bytes
        .get(8..68)
        .ok_or_else(|| format!("{} has no first member", path.display()))?;
    let name = header_field(header, 0, 16);
    let width = match name.as_str() {
        "/" => 4,
        "/SYM64/" => 8,
        other => {
            return Err(format!(
                "{}: first member is `{other}`, not the symbol index",
                path.display()
            ));
        }
    };
    let size: usize = header_field(header, 48, 58)
        .parse()
        .map_err(|e| format!("{}: unreadable member size: {e}", path.display()))?;
    let body = bytes
        .get(68..68 + size)
        .ok_or_else(|| format!("{}: symbol index is truncated", path.display()))?;
    let count_bytes = body
        .get(..width)
        .ok_or_else(|| format!("{}: symbol index has no count", path.display()))?;
    let count = count_bytes
        .iter()
        .fold(0usize, |acc, b| (acc << 8) | usize::from(*b));
    let names = body
        .get(width * (count + 1)..)
        .ok_or_else(|| format!("{}: symbol index names are truncated", path.display()))?;
    Ok(names
        .split(|b| *b == 0)
        .filter(|n| !n.is_empty())
        .map(|n| String::from_utf8_lossy(n).into_owned())
        .collect())
}

/// The C-ABI runtime surface a generated program links against.
fn runtime_symbols(path: &Path) -> BTreeSet<String> {
    archive_symbols(path)
        .unwrap_or_else(|e| panic!("{e}"))
        .into_iter()
        .filter(|s| s.starts_with("gos_rt_"))
        .collect()
}

/// A one-line report of the symbols one archive defines and the other does not.
fn missing_report(missing: &BTreeSet<String>) -> String {
    let shown: Vec<&str> = missing.iter().take(10).map(String::as_str).collect();
    let rest = missing.len().saturating_sub(shown.len());
    if rest == 0 {
        shown.join(", ")
    } else {
        format!("{} and {rest} more", shown.join(", "))
    }
}

/// Both archives an install publishes define the same `gos_rt_*` surface.
///
/// A build that refreshes one and leaves the other behind is what a release
/// link reports as an undefined symbol, in a program whose own source names
/// nothing new.
#[test]
fn both_runtime_archives_export_the_same_runtime_symbols() {
    let dir = artifact_dir();
    let host = dir.join("libgossamer_runtime.a");
    let musl = dir.join("libgossamer_runtime-musl.a");
    if !musl.exists() {
        eprintln!(
            "skipping: no static-musl runtime archive in {} (Linux hosts with \
             the musl target installed build one)",
            dir.display()
        );
        return;
    }
    assert!(
        host.exists(),
        "the static-musl runtime archive is published but the host one is not, \
         at {}",
        host.display()
    );

    let host_syms = runtime_symbols(&host);
    let musl_syms = runtime_symbols(&musl);

    // A parse that answered nothing would agree with itself, so the surface
    // has to be recognisably the runtime's before the comparison means
    // anything.
    assert!(
        host_syms.len() > 100,
        "{} names only {} gos_rt_* symbols; the archive index did not parse",
        host.display(),
        host_syms.len()
    );

    let only_host: BTreeSet<String> = host_syms.difference(&musl_syms).cloned().collect();
    let only_musl: BTreeSet<String> = musl_syms.difference(&host_syms).cloned().collect();
    assert!(
        only_host.is_empty() && only_musl.is_empty(),
        "the two runtime archives disagree on the gos_rt_* surface, so a \
         static-musl release link resolves a different set of symbols than a \
         host link.\n  only in {host}: {h}\n  only in {musl}: {m}\nRebuild both \
         archives from one tree and reinstall the pair.",
        host = host.display(),
        musl = musl.display(),
        h = missing_report(&only_host),
        m = missing_report(&only_musl),
    );
}
