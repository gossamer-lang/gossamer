//! Cross-compilation target registry.
//! Enumerates triples the driver can recognise for target-specific
//! compilation work. Registration is intentionally broader than the
//! supported execution contract; `conformance/target_matrix.tsv` is
//! authoritative for the latter. Each entry records the canonical triple,
//! its pointer width, and whether it needs a scheduler fallback (wasm32 has
//! no threads yet).

#![forbid(unsafe_code)]

use crate::link::TargetTriple;

/// Metadata attached to every registered target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetInfo {
    /// Canonical Rust triple (`x86_64-unknown-linux-gnu`, ...).
    pub triple: TargetTriple,
    /// Operating-system family (linux, macos, windows, freebsd, wasi).
    pub os: &'static str,
    /// CPU architecture family (`x86_64`, `aarch64`, `riscv64`, `wasm32`).
    pub arch: &'static str,
    /// Pointer width in bytes.
    pub pointer_width: u32,
    /// `true` when the target supports full M:N threading. `false`
    /// pins the scheduler to a single cooperative M (wasm32).
    pub multi_threaded: bool,
    /// Object-file flavour expected by the host linker.
    pub object_format: ObjectFormat,
}

/// Object-file flavours Gossamer emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectFormat {
    /// ELF (`.o`) on unix.
    Elf,
    /// Mach-O (`.o`) on macOS.
    MachO,
    /// COFF (`.obj`) on Windows.
    Coff,
    /// WebAssembly module (`.wasm`).
    Wasm,
}

impl ObjectFormat {
    /// Returns the conventional file extension for this format.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Elf | Self::MachO => "o",
            Self::Coff => "obj",
            Self::Wasm => "wasm",
        }
    }
}

/// Full list of triples recognised by the driver. Extending this list does
/// not make a target supported: it also requires the target-matrix evidence,
/// runtime archive, and release-pipeline work. `*-musl` triples are gated
/// behind the `musl` Cargo feature; off by default because most dev machines
/// do not have a musl sysroot installed.
#[cfg(not(feature = "musl"))]
pub const REGISTERED_TARGETS: &[(&str, &str, &str, u32, bool, ObjectFormat)] = &[
    (
        "x86_64-unknown-linux-gnu",
        "linux",
        "x86_64",
        8,
        true,
        ObjectFormat::Elf,
    ),
    (
        "aarch64-unknown-linux-gnu",
        "linux",
        "aarch64",
        8,
        true,
        ObjectFormat::Elf,
    ),
    (
        "x86_64-apple-darwin",
        "macos",
        "x86_64",
        8,
        true,
        ObjectFormat::MachO,
    ),
    (
        "aarch64-apple-darwin",
        "macos",
        "aarch64",
        8,
        true,
        ObjectFormat::MachO,
    ),
    (
        "x86_64-pc-windows-msvc",
        "windows",
        "x86_64",
        8,
        true,
        ObjectFormat::Coff,
    ),
    (
        "riscv64gc-unknown-linux-gnu",
        "linux",
        "riscv64",
        8,
        true,
        ObjectFormat::Elf,
    ),
    (
        "wasm32-unknown-unknown",
        "unknown",
        "wasm32",
        4,
        false,
        ObjectFormat::Wasm,
    ),
    (
        "wasm32-wasip1",
        "wasi",
        "wasm32",
        4,
        false,
        ObjectFormat::Wasm,
    ),
];

/// `musl`-gated copy of [`REGISTERED_TARGETS`]: appends the musl
/// triples to the default set. Compiled only when the `musl` Cargo
/// feature is on.
#[cfg(feature = "musl")]
pub const REGISTERED_TARGETS: &[(&str, &str, &str, u32, bool, ObjectFormat)] = &[
    (
        "x86_64-unknown-linux-gnu",
        "linux",
        "x86_64",
        8,
        true,
        ObjectFormat::Elf,
    ),
    (
        "x86_64-unknown-linux-musl",
        "linux",
        "x86_64",
        8,
        true,
        ObjectFormat::Elf,
    ),
    (
        "aarch64-unknown-linux-gnu",
        "linux",
        "aarch64",
        8,
        true,
        ObjectFormat::Elf,
    ),
    (
        "aarch64-unknown-linux-musl",
        "linux",
        "aarch64",
        8,
        true,
        ObjectFormat::Elf,
    ),
    (
        "x86_64-apple-darwin",
        "macos",
        "x86_64",
        8,
        true,
        ObjectFormat::MachO,
    ),
    (
        "aarch64-apple-darwin",
        "macos",
        "aarch64",
        8,
        true,
        ObjectFormat::MachO,
    ),
    (
        "x86_64-pc-windows-msvc",
        "windows",
        "x86_64",
        8,
        true,
        ObjectFormat::Coff,
    ),
    (
        "riscv64gc-unknown-linux-gnu",
        "linux",
        "riscv64",
        8,
        true,
        ObjectFormat::Elf,
    ),
    (
        "wasm32-unknown-unknown",
        "unknown",
        "wasm32",
        4,
        false,
        ObjectFormat::Wasm,
    ),
    (
        "wasm32-wasip1",
        "wasi",
        "wasm32",
        4,
        false,
        ObjectFormat::Wasm,
    ),
];

/// Iterates every registered [`TargetInfo`]. Order matches the source
/// table above.
pub fn all_targets() -> impl Iterator<Item = TargetInfo> {
    REGISTERED_TARGETS.iter().map(|entry| TargetInfo {
        triple: TargetTriple(entry.0.to_string()),
        os: entry.1,
        arch: entry.2,
        pointer_width: entry.3,
        multi_threaded: entry.4,
        object_format: entry.5,
    })
}

/// Returns the [`TargetInfo`] whose triple matches `name`, if any.
#[must_use]
pub fn lookup_target(name: &str) -> Option<TargetInfo> {
    all_targets().find(|info| info.triple.as_str() == name)
}

/// Prebuilt-runtime descriptor surfaced to the linker. Production
/// toolchains ship one of these per target; the driver picks the one
/// matching [`crate::link::LinkerOptions::target`] and passes it to
/// the link stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrebuiltRuntime {
    /// Target the runtime archive was compiled for.
    pub target: TargetTriple,
    /// Content-addressable identifier (sha256 fragment) for the
    /// archive. In tests we use a synthetic value; the production
    /// driver fills this from the manifest shipped with the toolchain.
    pub digest: String,
    /// Opaque archive payload.
    pub archive: Vec<u8>,
}

impl PrebuiltRuntime {
    /// Constructs a deterministic stub runtime for a given target.
    /// Used both by tests and by the first-party driver when the real
    /// archive is not yet installed.
    #[must_use]
    pub fn stub(target: TargetTriple) -> Self {
        let digest = format!("stub-{}", target.as_str());
        let archive = format!("gossamer-runtime:{}", target.as_str()).into_bytes();
        Self {
            target,
            digest,
            archive,
        }
    }
}
