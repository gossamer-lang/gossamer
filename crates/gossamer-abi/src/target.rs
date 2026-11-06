use std::fmt;

/// Operating system of a build target, named as rustc's `target_os` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Os {
    /// Linux with a GNU or musl C library.
    Linux,
    /// Android (bionic).
    Android,
    /// macOS.
    MacOs,
    /// iOS, device or simulator.
    Ios,
    /// FreeBSD.
    FreeBsd,
    /// NetBSD.
    NetBsd,
    /// OpenBSD.
    OpenBsd,
    /// Windows.
    Windows,
    /// WASI.
    Wasi,
    /// No operating system (`wasm32-unknown-unknown`) or one Gossamer does not know.
    Unknown,
}

impl Os {
    /// The `target_os` spelling of this OS.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::Android => "android",
            Self::MacOs => "macos",
            Self::Ios => "ios",
            Self::FreeBsd => "freebsd",
            Self::NetBsd => "netbsd",
            Self::OpenBsd => "openbsd",
            Self::Windows => "windows",
            Self::Wasi => "wasi",
            Self::Unknown => "unknown",
        }
    }

    /// The OS named by a `target_os` value, as `std::env::consts::OS` reports it.
    #[must_use]
    pub fn from_name(name: &str) -> Self {
        match name {
            "linux" => Self::Linux,
            "android" => Self::Android,
            "macos" => Self::MacOs,
            "ios" => Self::Ios,
            "freebsd" => Self::FreeBsd,
            "netbsd" => Self::NetBsd,
            "openbsd" => Self::OpenBsd,
            "windows" => Self::Windows,
            "wasi" => Self::Wasi,
            _ => Self::Unknown,
        }
    }

    /// The OS the toolchain itself runs on.
    #[must_use]
    pub fn host() -> Self {
        Self::from_name(std::env::consts::OS)
    }

    /// Whether this is one of Apple's operating systems (Mach-O objects, `-framework` links).
    #[must_use]
    pub const fn is_apple(self) -> bool {
        matches!(self, Self::MacOs | Self::Ios)
    }

    /// Whether this is FreeBSD, NetBSD, or OpenBSD.
    #[must_use]
    pub const fn is_bsd(self) -> bool {
        matches!(self, Self::FreeBsd | Self::NetBsd | Self::OpenBsd)
    }

    /// Whether native objects for this OS are ELF and linked by a Unix `cc`.
    #[must_use]
    pub const fn is_elf_unix(self) -> bool {
        matches!(
            self,
            Self::Linux | Self::Android | Self::FreeBsd | Self::NetBsd | Self::OpenBsd
        )
    }
}

impl fmt::Display for Os {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// C library and ABI environment of a build target, named as rustc's `target_env` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Env {
    /// glibc, or the GNU toolchain on Windows.
    Gnu,
    /// musl.
    Musl,
    /// The Microsoft toolchain.
    Msvc,
    /// No distinguished environment (macOS, the BSDs, Android).
    #[default]
    None,
}

impl Env {
    /// The `target_env` spelling, empty for [`Env::None`].
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Gnu => "gnu",
            Self::Musl => "musl",
            Self::Msvc => "msvc",
            Self::None => "",
        }
    }
}

/// A build target decomposed from its triple: the one place a triple is parsed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Target {
    triple: String,
    arch: String,
    os: Os,
    env: Env,
}

impl Target {
    /// Decomposes a Rust or LLVM target triple (`x86_64-unknown-freebsd`,
    /// `aarch64-linux-android21`, `arm64-apple-macosx15.0.0`).
    #[must_use]
    pub fn parse(triple: &str) -> Self {
        let mut parts = triple.split('-');
        let arch = normalize_arch(parts.next().unwrap_or_default()).to_string();
        let rest: Vec<&str> = parts.collect();
        // `linux` also appears in Android triples (`aarch64-linux-android`), so it yields to
        // any more specific component.
        let os = rest
            .iter()
            .filter_map(|part| os_component(part))
            .max_by_key(|os| *os != Os::Linux)
            .unwrap_or(Os::Unknown);
        let env = rest.last().map_or(Env::None, |last| env_component(last));
        Self {
            triple: triple.to_string(),
            arch,
            os,
            env,
        }
    }

    /// The target the toolchain runs on, under its canonical Rust triple.
    ///
    /// A Linux host is always `*-linux-gnu` and a Windows host `*-pc-windows-msvc`: native
    /// links use the system's own C toolchain, even when `gos` itself is a static musl build.
    #[must_use]
    pub fn host() -> Self {
        Self::parse(&host_triple(std::env::consts::ARCH, Os::host()))
    }

    /// The triple this target was parsed from.
    #[must_use]
    pub fn triple(&self) -> &str {
        &self.triple
    }

    /// The `target_arch` spelling (`x86_64`, `aarch64`, `riscv64`, `x86`, `arm`, `wasm32`).
    #[must_use]
    pub fn arch(&self) -> &str {
        &self.arch
    }

    /// The operating system.
    #[must_use]
    pub fn os(&self) -> Os {
        self.os
    }

    /// The C library environment.
    #[must_use]
    pub fn env(&self) -> Env {
        self.env
    }

    /// The `target_family` spelling: `unix`, `windows`, or `wasm`.
    #[must_use]
    pub fn family(&self) -> &'static str {
        match self.os {
            Os::Windows => "windows",
            Os::Wasi | Os::Unknown if self.arch.starts_with("wasm") => "wasm",
            _ => "unix",
        }
    }

    /// The `target_pointer_width` in bits.
    #[must_use]
    pub fn pointer_width(&self) -> u32 {
        match self.arch.as_str() {
            "x86" | "arm" | "riscv32" | "wasm32" => 32,
            _ => 64,
        }
    }

    /// Whether the target stores integers big-endian.
    #[must_use]
    pub fn is_big_endian(&self) -> bool {
        matches!(
            self.arch.as_str(),
            "powerpc" | "powerpc64" | "s390x" | "sparc64" | "mips" | "mips64"
        )
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.triple)
    }
}

/// The canonical Rust triple for `arch` (a `std::env::consts::ARCH` value) on `os`.
#[must_use]
pub fn host_triple(arch: &str, os: Os) -> String {
    let arch = if arch == "riscv64" { "riscv64gc" } else { arch };
    match os {
        Os::Linux => format!("{arch}-unknown-linux-gnu"),
        Os::Android => format!("{arch}-linux-android"),
        Os::MacOs => format!("{arch}-apple-darwin"),
        Os::Ios => format!("{arch}-apple-ios"),
        Os::FreeBsd => format!("{arch}-unknown-freebsd"),
        Os::NetBsd => format!("{arch}-unknown-netbsd"),
        Os::OpenBsd => format!("{arch}-unknown-openbsd"),
        Os::Windows => format!("{arch}-pc-windows-msvc"),
        Os::Wasi => format!("{arch}-wasip1"),
        Os::Unknown => format!("{arch}-unknown-unknown"),
    }
}

fn normalize_arch(arch: &str) -> &str {
    match arch {
        "arm64" => "aarch64",
        "i686" | "i586" => "x86",
        a if a.starts_with("riscv64") => "riscv64",
        a if a.starts_with("riscv32") => "riscv32",
        a if a.starts_with("armv7") || a.starts_with("thumbv7") => "arm",
        a => a,
    }
}

// Components may carry a version suffix: `android21`, `freebsd14.1`, `macosx15.0.0`, `ios17.0`.
fn os_component(part: &str) -> Option<Os> {
    let os = match part {
        "windows" => Os::Windows,
        "darwin" => Os::MacOs,
        "linux" => Os::Linux,
        p if p.starts_with("macos") => Os::MacOs,
        p if p.starts_with("ios") => Os::Ios,
        p if p.starts_with("android") => Os::Android,
        p if p.starts_with("freebsd") => Os::FreeBsd,
        p if p.starts_with("netbsd") => Os::NetBsd,
        p if p.starts_with("openbsd") => Os::OpenBsd,
        p if p.starts_with("wasi") => Os::Wasi,
        _ => return None,
    };
    Some(os)
}

fn env_component(last: &str) -> Env {
    if last.starts_with("musl") {
        Env::Musl
    } else if last.starts_with("gnu") {
        Env::Gnu
    } else if last == "msvc" {
        Env::Msvc
    } else {
        Env::None
    }
}

#[cfg(test)]
mod tests {
    use super::{Env, Os, Target, host_triple};

    /// Each triple with the `target_arch`, OS, `target_env`, and `target_family` rustc gives it.
    const CASES: &[(&str, &str, Os, Env, &str)] = &[
        (
            "x86_64-unknown-linux-gnu",
            "x86_64",
            Os::Linux,
            Env::Gnu,
            "unix",
        ),
        (
            "aarch64-unknown-linux-musl",
            "aarch64",
            Os::Linux,
            Env::Musl,
            "unix",
        ),
        (
            "riscv64gc-unknown-linux-gnu",
            "riscv64",
            Os::Linux,
            Env::Gnu,
            "unix",
        ),
        (
            "aarch64-apple-darwin",
            "aarch64",
            Os::MacOs,
            Env::None,
            "unix",
        ),
        (
            "arm64-apple-macosx15.0.0",
            "aarch64",
            Os::MacOs,
            Env::None,
            "unix",
        ),
        (
            "x86_64-pc-windows-msvc",
            "x86_64",
            Os::Windows,
            Env::Msvc,
            "windows",
        ),
        (
            "x86_64-pc-windows-gnu",
            "x86_64",
            Os::Windows,
            Env::Gnu,
            "windows",
        ),
        (
            "x86_64-unknown-freebsd",
            "x86_64",
            Os::FreeBsd,
            Env::None,
            "unix",
        ),
        (
            "aarch64-unknown-freebsd14.1",
            "aarch64",
            Os::FreeBsd,
            Env::None,
            "unix",
        ),
        (
            "x86_64-unknown-netbsd",
            "x86_64",
            Os::NetBsd,
            Env::None,
            "unix",
        ),
        (
            "x86_64-unknown-openbsd",
            "x86_64",
            Os::OpenBsd,
            Env::None,
            "unix",
        ),
        (
            "aarch64-linux-android",
            "aarch64",
            Os::Android,
            Env::None,
            "unix",
        ),
        (
            "aarch64-linux-android21",
            "aarch64",
            Os::Android,
            Env::None,
            "unix",
        ),
        ("aarch64-apple-ios", "aarch64", Os::Ios, Env::None, "unix"),
        (
            "arm64-apple-ios17.0-simulator",
            "aarch64",
            Os::Ios,
            Env::None,
            "unix",
        ),
        (
            "wasm32-unknown-unknown",
            "wasm32",
            Os::Unknown,
            Env::None,
            "wasm",
        ),
        ("wasm32-wasip1", "wasm32", Os::Wasi, Env::None, "wasm"),
    ];

    #[test]
    fn every_supported_triple_parses_to_rustc_cfg_values() {
        let cases = CASES.iter().copied();
        for (triple, arch, os, env, family) in cases {
            let target = Target::parse(triple);
            assert_eq!(target.arch(), arch, "{triple}");
            assert_eq!(target.os(), os, "{triple}");
            assert_eq!(target.env(), env, "{triple}");
            assert_eq!(target.family(), family, "{triple}");
        }
    }

    #[test]
    fn host_target_matches_the_running_toolchain() {
        let host = Target::host();
        assert_eq!(host.os().name(), std::env::consts::OS);
        assert_eq!(host.arch(), std::env::consts::ARCH);
        assert_eq!(host.family(), std::env::consts::FAMILY);
        assert_eq!(host.pointer_width(), usize::BITS);
    }

    #[test]
    fn host_triples_use_the_system_toolchain_spelling() {
        assert_eq!(host_triple("x86_64", Os::Linux), "x86_64-unknown-linux-gnu");
        assert_eq!(
            host_triple("riscv64", Os::Linux),
            "riscv64gc-unknown-linux-gnu"
        );
        assert_eq!(host_triple("aarch64", Os::MacOs), "aarch64-apple-darwin");
        assert_eq!(host_triple("x86_64", Os::Windows), "x86_64-pc-windows-msvc");
        assert_eq!(host_triple("x86_64", Os::FreeBsd), "x86_64-unknown-freebsd");
        assert_eq!(host_triple("aarch64", Os::Android), "aarch64-linux-android");
    }
}
