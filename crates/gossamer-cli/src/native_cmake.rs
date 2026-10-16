//! Building the `CMake` project a package's `[native.cmake]` table names.
//!
//! A native build links the project's static libraries; the bytecode VM and
//! the JIT load its shared ones. Each linkage is configured once into its own
//! build directory under the per-user cache's `native/` root, keyed on the
//! source directory, the options, the target, and the C compiler, so every
//! project that depends on one checkout of the package shares its build. It
//! is then rebuilt and installed on every use: `CMake` decides what is stale,
//! and a current build costs one no-op pass.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow};
use gossamer_pkg::CmakeSpec;

/// How the libraries a build installs are linked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Linkage {
    /// Static archives, for a native build.
    Static,
    /// Shared libraries, for the bytecode VM and the JIT to load.
    Shared,
}

impl Linkage {
    fn name(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::Shared => "shared",
        }
    }
}

/// The compiler and target a build is configured for.
pub(crate) struct CmakeTarget<'a> {
    /// The target triple the libraries are built for.
    pub target: &'a str,
    /// Whether that target is the host.
    pub host: bool,
    /// The C compiler to configure with: the one the program's own native
    /// sources build with, so every object a link takes comes from one
    /// toolchain.
    pub compiler: &'a Path,
    /// The environment that compiler runs in (MSVC's `INCLUDE`, `LIB`, and
    /// `PATH`), which every `CMake` step that compiles needs too.
    pub env: &'a [(std::ffi::OsString, std::ffi::OsString)],
    /// The compiler's own report of what it is, part of the build's key.
    pub identity: &'a str,
    /// Whether the compiler is MSVC, which names its archives `<name>.lib`.
    pub msvc: bool,
}

/// What an installed build gives the link.
#[derive(Debug, Clone, Default)]
pub(crate) struct CmakeInstall {
    /// Directories holding the installed libraries, searched before the
    /// system's so a provided name resolves to this build.
    pub dirs: Vec<PathBuf>,
    /// Each provided library's static archive, for a static install.
    pub archives: Vec<(String, PathBuf)>,
    /// What those archives link against in turn, for a static install.
    pub dependencies: LinkDependencies,
}

/// The libraries, frameworks, and search directories a static archive
/// needs beside itself, as its project's pkg-config file records them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkDependencies {
    /// Libraries, by the name `-l` takes.
    pub libraries: Vec<String>,
    /// macOS frameworks.
    pub frameworks: Vec<String>,
    /// Directories searched for the libraries.
    pub search: Vec<PathBuf>,
}

impl LinkDependencies {
    /// Adds `other`'s entries this one does not hold yet.
    pub(crate) fn merge(&mut self, other: LinkDependencies) {
        for library in other.libraries {
            if !self.libraries.contains(&library) {
                self.libraries.push(library);
            }
        }
        for framework in other.frameworks {
            if !self.frameworks.contains(&framework) {
                self.frameworks.push(framework);
            }
        }
        for dir in other.search {
            if !self.search.contains(&dir) {
                self.search.push(dir);
            }
        }
    }
}

/// Configures (once), builds, and installs `spec` of the package rooted at
/// `root` with `linkage`, in the per-user cache.
pub(crate) fn install(
    root: &Path,
    spec: &CmakeSpec,
    linkage: Linkage,
    target: &CmakeTarget<'_>,
) -> Result<CmakeInstall> {
    let cmake = which::which("cmake").map_err(|_| {
        anyhow!(
            "the [native.cmake] build of `{}` needs `cmake` on PATH",
            spec.source
        )
    })?;
    let source = root.join(&spec.source);
    if !source.join("CMakeLists.txt").is_file() {
        return Err(anyhow!(
            "[native.cmake] source `{}` holds no CMakeLists.txt",
            source.display()
        ));
    }
    if !target.host && std::env::var_os("CMAKE_TOOLCHAIN_FILE").is_none() {
        return Err(anyhow!(
            "building the [native.cmake] project `{}` for `{}` needs a CMake toolchain file \
             for that target: set CMAKE_TOOLCHAIN_FILE",
            spec.source,
            target.target
        ));
    }
    let dir = gossamer_driver::frontend_cache::user_cache_root()
        .join("native")
        .join(format!(
            "cmake-{}-{}",
            linkage.name(),
            build_key(&source, spec, linkage, target)
        ));
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    // Builds sharing this directory, from parallel `gos` processes, take
    // turns: CMake and its generators do not share a build tree.
    let lock = fs::File::create(dir.join("lock"))
        .with_context(|| format!("creating {}", dir.join("lock").display()))?;
    lock.lock()
        .with_context(|| format!("locking {}", dir.display()))?;
    let build = dir.join("build");
    let prefix = dir.join("prefix");
    if !build.join("CMakeCache.txt").is_file() {
        eprintln!(
            "native: configuring the CMake project `{}` ({})",
            spec.source,
            linkage.name()
        );
        let mut cmd = Command::new(&cmake);
        cmd.envs(target.env.iter().cloned());
        cmd.arg("-S").arg(&source).arg("-B").arg(&build);
        cmd.args(configure_args(spec, linkage, target, &prefix));
        if which::which("ninja").is_ok() {
            cmd.arg("-G").arg("Ninja");
        }
        if let Err(err) = run(cmd, "configuring the CMake project") {
            let _ = fs::remove_dir_all(&build);
            return Err(err);
        }
    }
    let mut cmd = Command::new(&cmake);
    cmd.envs(target.env.iter().cloned());
    cmd.arg("--build")
        .arg(&build)
        .args(["--config", "Release", "--parallel"]);
    run(cmd, "building the CMake project")?;
    let mut cmd = Command::new(&cmake);
    cmd.envs(target.env.iter().cloned());
    cmd.arg("--install")
        .arg(&build)
        .args(["--config", "Release"]);
    run(cmd, "installing the CMake project")?;
    let lib = prefix.join("lib");
    let bin = prefix.join("bin");
    match linkage {
        Linkage::Static => {
            let link = static_link_dir(&lib, &dir.join("link"), spec, target.msvc)?;
            let archives = spec
                .provides
                .iter()
                .map(|name| (name.clone(), link.join(static_file_name(name, target.msvc))))
                .collect();
            let dependencies = installed_dependencies(&lib, spec)?;
            Ok(CmakeInstall {
                dirs: vec![link],
                archives,
                dependencies,
            })
        }
        Linkage::Shared => {
            for name in &spec.provides {
                let file = shared_file_name(name);
                if !lib.join(&file).is_file() && !bin.join(&file).is_file() {
                    return Err(anyhow!(
                        "the [native.cmake] build of `{}` installed no shared library `{file}`",
                        spec.source
                    ));
                }
            }
            Ok(CmakeInstall {
                dirs: [lib, bin].into_iter().filter(|dir| dir.is_dir()).collect(),
                archives: Vec::new(),
                dependencies: LinkDependencies::default(),
            })
        }
    }
}

/// The cache entries every configuration passes, then the package's own.
fn configure_args(
    spec: &CmakeSpec,
    linkage: Linkage,
    target: &CmakeTarget<'_>,
    prefix: &Path,
) -> Vec<String> {
    let mut args = vec![
        "-DCMAKE_BUILD_TYPE=Release".to_string(),
        format!(
            "-DBUILD_SHARED_LIBS={}",
            if linkage == Linkage::Shared {
                "ON"
            } else {
                "OFF"
            }
        ),
        "-DCMAKE_POSITION_INDEPENDENT_CODE=ON".to_string(),
        format!("-DCMAKE_INSTALL_PREFIX={}", prefix.display()),
        "-DCMAKE_INSTALL_LIBDIR=lib".to_string(),
        "-DCMAKE_INSTALL_BINDIR=bin".to_string(),
    ];
    args.push(format!("-DCMAKE_C_COMPILER={}", target.compiler.display()));
    args.extend(
        spec.options
            .iter()
            .map(|(name, value)| format!("-D{name}={value}")),
    );
    args
}

/// The build directory's key: what its configuration depends on.
fn build_key(
    source: &Path,
    spec: &CmakeSpec,
    linkage: Linkage,
    target: &CmakeTarget<'_>,
) -> String {
    let source = std::path::absolute(source).unwrap_or_else(|_| source.to_path_buf());
    let mut hasher = gossamer_pkg::sha256::Hasher::new();
    let mut part = |bytes: &[u8]| {
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    };
    part(source.to_string_lossy().as_bytes());
    part(linkage.name().as_bytes());
    part(target.target.as_bytes());
    part(target.identity.as_bytes());
    for (name, value) in &spec.options {
        part(name.as_bytes());
        part(value.as_bytes());
    }
    let hex = hasher.finalize_hex();
    hex[..16].to_string()
}

/// The directory a native link searches for the provided static archives,
/// each under the name `-l<name>` (or `<name>.lib`) finds.
///
/// A `CMake` project may name its static archive differently from the
/// library (`<name>-static.lib` is common with MSVC); such an archive is
/// copied under the linkable name into `link`.
fn static_link_dir(lib: &Path, link: &Path, spec: &CmakeSpec, msvc: bool) -> Result<PathBuf> {
    let mut renamed = Vec::new();
    for name in &spec.provides {
        let canonical = static_file_name(name, msvc);
        if lib.join(&canonical).is_file() {
            continue;
        }
        let alternatives = [
            format!("{name}-static.lib"),
            format!("lib{name}-static.a"),
            format!("lib{name}.a"),
        ];
        let Some(found) = alternatives
            .iter()
            .map(|file| lib.join(file))
            .find(|path| path.is_file())
        else {
            return Err(anyhow!(
                "the [native.cmake] build of `{}` installed no static library `{canonical}` \
                 under {}",
                spec.source,
                lib.display()
            ));
        };
        renamed.push((found, canonical));
    }
    if renamed.is_empty() {
        return Ok(lib.to_path_buf());
    }
    fs::create_dir_all(link).with_context(|| format!("creating {}", link.display()))?;
    for (found, canonical) in renamed {
        fs::copy(&found, link.join(&canonical))
            .with_context(|| format!("copying {}", found.display()))?;
    }
    Ok(link.to_path_buf())
}

/// What the static archives `spec` provides link against, read from the
/// pkg-config file the project installed for each under `lib`. A library
/// without one records nothing beyond itself.
fn installed_dependencies(lib: &Path, spec: &CmakeSpec) -> Result<LinkDependencies> {
    let mut out = LinkDependencies::default();
    for name in &spec.provides {
        let dir = lib.join("pkgconfig");
        let Some(file) = [name.clone(), name.to_lowercase()]
            .iter()
            .map(|stem| dir.join(format!("{stem}.pc")))
            .find(|path| path.is_file())
        else {
            continue;
        };
        let text =
            fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
        let record = PkgConfig::parse(&text, &dir);
        let mut tokens = record.libs;
        if !record.requires.is_empty() {
            tokens.extend(system_pkg_config_libs(&record.requires, &file)?);
        }
        out.merge(
            link_dependencies(&tokens, &spec.provides)
                .with_context(|| format!("reading the link options of {}", file.display()))?,
        );
    }
    Ok(out)
}

/// The `Libs` options and `Requires.private` packages of a pkg-config file,
/// its variables expanded.
#[derive(Debug, Default, PartialEq, Eq)]
struct PkgConfig {
    libs: Vec<String>,
    requires: Vec<String>,
}

impl PkgConfig {
    fn parse(text: &str, pcfiledir: &Path) -> Self {
        let mut variables = vec![(
            "pcfiledir".to_string(),
            pcfiledir.to_string_lossy().into_owned(),
        )];
        let mut out = Self::default();
        for line in text.lines().map(str::trim) {
            if let Some((key, value)) = line.split_once(':')
                && !key.contains('=')
            {
                let value = expand(value.trim(), &variables);
                match key.trim() {
                    "Libs" | "Libs.private" => {
                        out.libs
                            .extend(value.split_whitespace().map(ToString::to_string));
                    }
                    "Requires.private" => out.requires.extend(
                        value
                            .split([',', ' '])
                            .filter(|word| !word.is_empty())
                            .filter(|word| !word.starts_with(['<', '>', '=']))
                            .filter(|word| !word.starts_with(|c: char| c.is_ascii_digit()))
                            .map(ToString::to_string),
                    ),
                    _ => {}
                }
            } else if let Some((name, value)) = line.split_once('=')
                && !name.trim().is_empty()
                && !name.contains(char::is_whitespace)
            {
                let value = expand(value.trim(), &variables);
                variables.push((name.trim().to_string(), value));
            }
        }
        out
    }
}

/// `text` with each `${name}` replaced by its value in `variables`.
fn expand(text: &str, variables: &[(String, String)]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let name = &after[..end];
        match variables.iter().rev().find(|(known, _)| known == name) {
            Some((_, value)) => out.push_str(value),
            None => out.push_str(&rest[start..=start + 2 + end]),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// The link options the system's pkg-config gives the packages `requires`
/// names, which a static archive's own file depends on.
fn system_pkg_config_libs(requires: &[String], file: &Path) -> Result<Vec<String>> {
    let pkg_config = which::which("pkg-config").map_err(|_| {
        anyhow!(
            "{} requires the pkg-config packages {}, and `pkg-config` is not on PATH",
            file.display(),
            requires.join(", ")
        )
    })?;
    let out = Command::new(pkg_config)
        .args(["--libs", "--static"])
        .args(requires)
        .output()
        .with_context(|| format!("running pkg-config for {}", requires.join(", ")))?;
    if !out.status.success() {
        return Err(anyhow!(
            "pkg-config could not resolve {} for {}:\n{}",
            requires.join(", "),
            file.display(),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .map(ToString::to_string)
        .collect())
}

/// The libraries, frameworks, and search directories pkg-config link
/// options name, leaving out the archives `provides` names themselves.
fn link_dependencies(tokens: &[String], provides: &[String]) -> Result<LinkDependencies> {
    let is_own_archive = |name: &str| {
        provides
            .iter()
            .any(|own| name == own || name.strip_suffix("-static") == Some(own))
    };
    let mut out = LinkDependencies::default();
    let mut words = tokens.iter().map(String::as_str);
    while let Some(word) = words.next() {
        let framework = if let Some(name) = word.strip_prefix("-Wl,-framework,") {
            Some(name.to_string())
        } else if let Some(name) = word.strip_prefix("-Wl,-weak_framework,") {
            Some(name.to_string())
        } else if matches!(word, "-framework" | "-weak_framework") {
            Some(
                words
                    .next()
                    .ok_or_else(|| anyhow!("`{word}` names no framework"))?
                    .to_string(),
            )
        } else {
            None
        };
        if let Some(name) = framework {
            out.merge(LinkDependencies {
                frameworks: vec![name],
                ..LinkDependencies::default()
            });
        } else if let Some(name) = word.strip_prefix("-l") {
            if !is_own_archive(name) {
                out.merge(LinkDependencies {
                    libraries: vec![name.to_string()],
                    ..LinkDependencies::default()
                });
            }
        } else if word == "-pthread" {
            out.merge(LinkDependencies {
                libraries: vec!["pthread".to_string()],
                ..LinkDependencies::default()
            });
        } else if let Some(dir) = word.strip_prefix("-L") {
            out.merge(LinkDependencies {
                search: vec![PathBuf::from(dir)],
                ..LinkDependencies::default()
            });
        } else if word.starts_with("-Wl,-rpath") || word.starts_with("-Wl,--enable-new-dtags") {
            // A run-time search path names where a shared library loads
            // from; a static archive has none.
        } else {
            return Err(anyhow!(
                "the link option `{word}` is not one a native build passes on; it takes \
                 `-l`, `-L`, `-pthread`, and framework options"
            ));
        }
    }
    Ok(out)
}

fn static_file_name(name: &str, msvc: bool) -> String {
    if msvc {
        format!("{name}.lib")
    } else {
        format!("lib{name}.a")
    }
}

fn shared_file_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.dll")
    } else if cfg!(target_os = "macos") {
        format!("lib{name}.dylib")
    } else {
        format!("lib{name}.so")
    }
}

/// Every file under the project's source directory, outside version-control
/// metadata: a native build is current only while none of them changes.
pub(crate) fn source_files(root: &Path, spec: &CmakeSpec) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() && entry.file_name() != ".git" => walk(&path, out),
                Ok(kind) if kind.is_file() => out.push(path),
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(&root.join(&spec.source), &mut out);
    out.sort();
    out
}

fn run(mut cmd: Command, what: &str) -> Result<()> {
    let out = cmd
        .output()
        .with_context(|| format!("{what}: running {}", cmd.get_program().to_string_lossy()))?;
    if out.status.success() {
        return Ok(());
    }
    Err(anyhow!(
        "{what} failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(provides: &[&str]) -> CmakeSpec {
        CmakeSpec {
            source: "vendor".to_string(),
            provides: provides.iter().map(ToString::to_string).collect(),
            options: [("SDL_AUDIO".to_string(), "OFF".to_string())].into(),
            feature: None,
        }
    }

    fn tokens(text: &str) -> Vec<String> {
        text.split_whitespace().map(ToString::to_string).collect()
    }

    #[test]
    fn a_pkg_config_file_expands_its_variables_into_its_link_options() {
        let text = "prefix=/opt/sdl\nexec_prefix=${prefix}\nlibdir=${prefix}/lib\n\n\
                    Name: sdl3\nRequires.private: libdecor-0 >= 0.1, xkbcommon\n\
                    Libs: -L${libdir} -lSDL3 -pthread -lm\n";
        let record = PkgConfig::parse(text, Path::new("/opt/sdl/lib/pkgconfig"));
        assert_eq!(record.libs, tokens("-L/opt/sdl/lib -lSDL3 -pthread -lm"));
        assert_eq!(record.requires, ["libdecor-0", "xkbcommon"]);
    }

    #[test]
    fn link_options_name_libraries_frameworks_and_search_dirs_but_not_the_archive() {
        let deps = link_dependencies(
            &tokens(
                "-L/opt/sdl/lib -lSDL3-static -lm -pthread -Wl,-framework,Cocoa \
                 -Wl,-weak_framework,UniformTypeIdentifiers -framework Metal -lm \
                 -Wl,-rpath,/opt/sdl/lib",
            ),
            &["SDL3".to_string()],
        )
        .expect("every option is one a native build passes on");
        assert_eq!(deps.libraries, ["m", "pthread"]);
        assert_eq!(
            deps.frameworks,
            ["Cocoa", "UniformTypeIdentifiers", "Metal"]
        );
        assert_eq!(deps.search, [PathBuf::from("/opt/sdl/lib")]);
    }

    #[test]
    fn a_link_option_a_native_build_cannot_pass_is_refused() {
        let err = link_dependencies(&tokens("-lSDL3 -Wl,--whole-archive"), &["SDL3".to_string()])
            .expect_err("an unknown option is refused");
        assert!(err.to_string().contains("--whole-archive"), "{err}");
    }

    #[test]
    fn configuration_selects_the_linkage_and_passes_the_options() {
        let target = CmakeTarget {
            target: "x86_64-unknown-linux-gnu",
            host: true,
            compiler: Path::new("/usr/bin/cc"),
            env: &[],
            identity: "cc",
            msvc: false,
        };
        let args = configure_args(&spec(&["SDL3"]), Linkage::Static, &target, Path::new("/p"));
        assert!(args.contains(&"-DBUILD_SHARED_LIBS=OFF".to_string()));
        assert!(args.contains(&"-DCMAKE_INSTALL_PREFIX=/p".to_string()));
        assert!(args.contains(&"-DCMAKE_C_COMPILER=/usr/bin/cc".to_string()));
        assert_eq!(args.last().map(String::as_str), Some("-DSDL_AUDIO=OFF"));
        let shared = configure_args(&spec(&["SDL3"]), Linkage::Shared, &target, Path::new("/p"));
        assert!(shared.contains(&"-DBUILD_SHARED_LIBS=ON".to_string()));
    }

    #[test]
    fn a_differently_named_static_archive_is_linked_under_its_library_name() {
        let root = std::env::temp_dir().join(format!("gos-cmake-link-{}", std::process::id()));
        let lib = root.join("lib");
        fs::create_dir_all(&lib).expect("lib dir");
        fs::write(lib.join("SDL3-static.lib"), b"archive").expect("archive");
        let dir = static_link_dir(&lib, &root.join("link"), &spec(&["SDL3"]), true)
            .expect("a link directory");
        assert_eq!(dir, root.join("link"));
        assert!(dir.join("SDL3.lib").is_file());
        fs::write(lib.join("libz.a"), b"archive").expect("archive");
        assert_eq!(
            static_link_dir(&lib, &root.join("link2"), &spec(&["z"]), false).expect("lib dir"),
            lib
        );
        assert!(static_link_dir(&lib, &root.join("link3"), &spec(&["missing"]), false).is_err());
        let _ = fs::remove_dir_all(&root);
    }
}
