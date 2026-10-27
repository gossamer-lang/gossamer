//! The shared library `gos run` loads a program's `[rust-bindings]` as.
//!
//! A program's bindings, its own and its dependencies', build as one
//! `cdylib`, so crates two bindings share are one copy in the process as they
//! are in a native build's link. The set builds once per source, feature
//! set, target, and binding ABI into the user cache
//! (`<cache>/gossamer/bindings/<label>-<key>/`), shared by every project
//! naming the same set. A build lands under a name its sources' fingerprint
//! picks, beside the
//! `signatures.json` the host writes for it, so a running program keeps the
//! library it loaded while another process builds new sources. A warm start
//! fingerprints a path binding's sources and finds the build by that name;
//! it runs no Cargo.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use gossamer_binding_abi::{ABI_VERSION, SIGNATURE_HASH_SEED, signature_hash};
use gossamer_pkg::RustBindingSpec;

use crate::binding_build::{
    AdvisoryLock, BindingBuildError, CargoBuild, Profile, TOOLCHAIN_LOCK, binding_crates_patch,
    build_lock_path, cache_root, path_dependencies, render_one, run_cargo_build, seed_lockfile,
    toml_path_kv, write_if_different,
};

/// The directory, inside a path binding's crate, that `gos publish` stages
/// the binding's library in, one subdirectory per target.
pub const PREBUILT_DIR: &str = "gos-prebuilt";

/// The library of a program's bindings, built or found in the cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingLibrary {
    /// The shared library.
    pub library: PathBuf,
    /// Where its signatures are cached; absent until the host records them.
    pub signatures: PathBuf,
}

impl BindingLibrary {
    /// Whether the signatures of this build are recorded.
    #[must_use]
    pub fn has_signatures(&self) -> bool {
        self.signatures.is_file()
    }
}

/// The Rust identifier Cargo makes of a crate name.
fn crate_ident(name: &str) -> String {
    name.replace('-', "_")
}

fn hex(hash: u64) -> String {
    format!("{hash:016x}")
}

/// The Cargo library name of every set's wrapper crate.
const LIBRARY_NAME: &str = "gos_bindings";

/// One binding of a set, as `[rust-bindings]` declares it.
#[derive(Debug, Clone)]
struct Member {
    name: String,
    dep_line: String,
    /// The crate's directory, for a binding whose sources are on disk.
    local_root: Option<PathBuf>,
    /// Whether it is declared by `path`, the shape a prebuilt library covers.
    by_path: bool,
}

fn members(bindings: &BTreeMap<String, RustBindingSpec>, manifest_dir: &Path) -> Vec<Member> {
    bindings
        .iter()
        .map(|(name, spec)| {
            let (dep_line, _features, local_root) = render_one(name, spec, manifest_dir);
            Member {
                name: name.clone(),
                dep_line,
                local_root,
                by_path: matches!(spec, RustBindingSpec::Path { .. }),
            }
        })
        .collect()
}

/// The cache directory of `members` built for `target`, under `cache_root`.
fn library_dir(cache_root: &Path, members: &[Member], target: Option<&str>) -> PathBuf {
    let mut key = SIGNATURE_HASH_SEED;
    for member in members {
        key = signature_hash(key, member.name.as_bytes());
        key = signature_hash(key, member.dep_line.as_bytes());
    }
    for part in [target.unwrap_or("host"), &ABI_VERSION.to_string()] {
        key = signature_hash(key, part.as_bytes());
    }
    let label = match members {
        [one] => crate_ident(&one.name),
        [first, rest @ ..] => format!("{}+{}", crate_ident(&first.name), rest.len()),
        [] => "none".to_string(),
    };
    cache_root
        .join("bindings")
        .join(format!("{label}-{}", hex(key)))
}

/// Folds the size and modification time of every source file under `root`
/// and the crates it names by path into `hash`. Build output and hidden
/// directories are skipped.
fn fingerprint_tree(root: &Path, hash: &mut u64, seen: &mut Vec<PathBuf>) -> io::Result<()> {
    let root = fs::canonicalize(root)?;
    if seen.contains(&root) {
        return Ok(());
    }
    seen.push(root.clone());
    let mut files = Vec::new();
    collect_files(&root, &mut files)?;
    files.sort();
    for file in &files {
        let meta = fs::metadata(file)?;
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        let relative = file.strip_prefix(&root).unwrap_or(file);
        *hash = signature_hash(*hash, relative.to_string_lossy().as_bytes());
        *hash = signature_hash(*hash, format!("{}:{modified}", meta.len()).as_bytes());
    }
    for dep in path_dependencies(&root.join("Cargo.toml")) {
        if dep.is_dir() {
            fingerprint_tree(&dep, hash, seen)?;
        }
    }
    Ok(())
}

/// Folds the relative path and bytes of every source file under `root` and
/// the crates it names by path into `hash`, which reads the same wherever
/// the sources are unpacked.
fn content_tree(root: &Path, hash: &mut u64, seen: &mut Vec<PathBuf>) -> io::Result<()> {
    let root = fs::canonicalize(root)?;
    if seen.contains(&root) {
        return Ok(());
    }
    seen.push(root.clone());
    let mut files = Vec::new();
    collect_files(&root, &mut files)?;
    files.sort();
    for file in &files {
        let relative = file.strip_prefix(&root).unwrap_or(file);
        *hash = signature_hash(*hash, relative.to_string_lossy().as_bytes());
        *hash = signature_hash(*hash, &fs::read(file)?);
    }
    for dep in path_dependencies(&root.join("Cargo.toml")) {
        if dep.is_dir() {
            content_tree(&dep, hash, seen)?;
        }
    }
    Ok(())
}

/// The target a library must be built for to load into this process.
fn host_target() -> String {
    let env = if cfg!(target_env = "musl") {
        "-musl"
    } else if cfg!(target_env = "gnu") {
        "-gnu"
    } else if cfg!(target_env = "msvc") {
        "-msvc"
    } else {
        ""
    };
    format!("{}-{}{env}", std::env::consts::ARCH, std::env::consts::OS)
}

/// The key a prebuilt library of `members` is staged under, when every one
/// is a path binding: it covers their sources, features, target, and binding
/// ABI, so the library stands in only for a build of the same set.
fn prebuilt_key(members: &[Member]) -> io::Result<Option<String>> {
    let mut key = SIGNATURE_HASH_SEED;
    for part in [&host_target(), &ABI_VERSION.to_string()] {
        key = signature_hash(key, part.as_bytes());
    }
    for member in members {
        let (true, Some(root)) = (member.by_path, member.local_root.as_deref()) else {
            return Ok(None);
        };
        // The crate's own location differs on every machine; its contents decide.
        let portable = member.dep_line.replace(&toml_path_kv("path", root), "");
        key = signature_hash(key, member.name.as_bytes());
        key = signature_hash(key, portable.as_bytes());
        content_tree(root, &mut key, &mut Vec::new())?;
    }
    Ok(Some(hex(key)))
}

fn prebuilt_path(root: &Path, key: &str) -> PathBuf {
    root.join(PREBUILT_DIR).join(host_target()).join(format!(
        "{LIBRARY_NAME}-{key}{}",
        std::env::consts::DLL_SUFFIX
    ))
}

/// Installs a library a member's crate carries for exactly this set as
/// `target`. Answers whether it did.
fn take_prebuilt(members: &[Member], target: &Path) -> io::Result<bool> {
    let Some(key) = prebuilt_key(members)? else {
        return Ok(false);
    };
    let Some(library) = members
        .iter()
        .filter_map(|member| member.local_root.as_deref())
        .map(|root| prebuilt_path(root, &key))
        .find(|path| path.is_file())
    else {
        return Ok(false);
    };
    let staged = target.with_extension(format!("{}.part", std::process::id()));
    fs::copy(&library, &staged)?;
    fs::rename(&staged, target)?;
    Ok(true)
}

/// Builds the path bindings of `bindings` as one library and stages it in
/// the first one's crate under [`PREBUILT_DIR`], for `gos publish` to pack,
/// so a program whose bindings are this set loads it on this target without
/// a Rust toolchain. Answers the staged library, or `None` when the set holds
/// no path binding.
pub fn stage_prebuilt(
    bindings: &BTreeMap<String, RustBindingSpec>,
    manifest_dir: &Path,
) -> Result<Option<PathBuf>, BindingBuildError> {
    let by_path: BTreeMap<String, RustBindingSpec> = bindings
        .iter()
        .filter(|(_, spec)| matches!(spec, RustBindingSpec::Path { .. }))
        .map(|(name, spec)| (name.clone(), spec.clone()))
        .collect();
    let manifest_dir = std::path::absolute(manifest_dir)?;
    let set = members(&by_path, &manifest_dir);
    let (Some(key), Some(root)) = (
        prebuilt_key(&set)?,
        set.first().and_then(|member| member.local_root.clone()),
    ) else {
        return Ok(None);
    };
    let built = ensure_bindings_library(&by_path, &manifest_dir)?;
    let library = prebuilt_path(&root, &key);
    if let Some(dir) = library.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::copy(&built.library, &library)?;
    Ok(Some(library))
}

/// Raises `newest` to the modification time of every source file under
/// `root` and the crates it names by path.
pub(crate) fn newest_in_tree(
    root: &Path,
    newest: &mut Option<std::time::SystemTime>,
    seen: &mut Vec<PathBuf>,
) -> io::Result<()> {
    let root = fs::canonicalize(root)?;
    if seen.contains(&root) {
        return Ok(());
    }
    seen.push(root.clone());
    let mut files = Vec::new();
    collect_files(&root, &mut files)?;
    for file in files {
        if let Ok(modified) = fs::metadata(&file).and_then(|meta| meta.modified())
            && newest.is_none_or(|n| modified > n)
        {
            *newest = Some(modified);
        }
    }
    for dep in path_dependencies(&root.join("Cargo.toml")) {
        if dep.is_dir() {
            newest_in_tree(&dep, newest, seen)?;
        }
    }
    Ok(())
}

fn collect_files(dir: &Path, files: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name == "target" || name == PREBUILT_DIR {
            continue;
        }
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect_files(&path, files)?;
        } else {
            files.push(path);
        }
    }
    Ok(())
}

/// The name a build of these sources, under the manifest that builds them,
/// lands under.
/// A fingerprint of the Rust sources behind `bindings`: every file of each
/// local binding crate and of the crates those name by path, by relative
/// path, size and modification time. It changes whenever an edit to any of
/// them changes what a build links.
pub fn bindings_source_fingerprint(
    bindings: &BTreeMap<String, RustBindingSpec>,
    manifest_dir: &Path,
) -> io::Result<String> {
    let manifest_dir = std::path::absolute(manifest_dir)?;
    let set = members(bindings, &manifest_dir);
    let mut hash = signature_hash(SIGNATURE_HASH_SEED, wrapper_manifest(&set).as_bytes());
    let mut seen = Vec::new();
    for root in set.iter().filter_map(|m| m.local_root.as_deref()) {
        fingerprint_tree(root, &mut hash, &mut seen)?;
    }
    Ok(hex(hash))
}

fn build_stem(dir: &Path, members: &[Member]) -> io::Result<String> {
    let mut hash = signature_hash(SIGNATURE_HASH_SEED, dir.to_string_lossy().as_bytes());
    hash = signature_hash(hash, wrapper_manifest(members).as_bytes());
    for root in members.iter().filter_map(|m| m.local_root.as_deref()) {
        fingerprint_tree(root, &mut hash, &mut Vec::new())?;
    }
    Ok(format!("{LIBRARY_NAME}-{}", hex(hash)))
}

fn library_path(dir: &Path, stem: &str) -> PathBuf {
    dir.join("lib")
        .join(format!("{stem}{}", std::env::consts::DLL_SUFFIX))
}

/// The library of the bindings a program in the project at `manifest_dir`
/// names, building it with Cargo when their sources changed or no build
/// exists. Cargo's progress and diagnostics stream to stderr.
pub fn ensure_bindings_library(
    bindings: &BTreeMap<String, RustBindingSpec>,
    manifest_dir: &Path,
) -> Result<BindingLibrary, BindingBuildError> {
    ensure_bindings_library_in(bindings, manifest_dir, &cache_root()?)
}

/// [`ensure_bindings_library`] with an explicit cache root instead of the
/// one `GOSSAMER_CACHE` / `XDG_CACHE_HOME` name.
pub fn ensure_bindings_library_in(
    bindings: &BTreeMap<String, RustBindingSpec>,
    manifest_dir: &Path,
    cache_root: &Path,
) -> Result<BindingLibrary, BindingBuildError> {
    // The wrapper crate builds elsewhere, so a path it names must be absolute.
    let manifest_dir = std::path::absolute(manifest_dir)?;
    let set = members(bindings, &manifest_dir);
    let dir = library_dir(cache_root, &set, None);
    let stem = build_stem(&dir, &set)?;
    let built = BindingLibrary {
        library: library_path(&dir, &stem),
        signatures: dir.join("lib").join(format!("{stem}.json")),
    };
    if built.library.is_file() {
        return Ok(built);
    }
    fs::create_dir_all(dir.join("lib"))?;
    let crates: Vec<&str> = set.iter().map(|member| member.name.as_str()).collect();
    let _lock = AdvisoryLock::acquire(
        &build_lock_path(&dir),
        &format!("the shared library of Rust bindings {}", crates.join(", ")),
    )?;
    // Another process may have built these sources while this one waited.
    if built.library.is_file() {
        return Ok(built);
    }
    // A binding's crate may carry a library its publisher built from this
    // same set, which saves the Rust toolchain a build.
    if take_prebuilt(&set, &built.library)? {
        prune_other_builds(&dir, &stem);
        return Ok(built);
    }
    let project = dir.join("build");
    fs::create_dir_all(&project)?;
    write_if_different(&project.join("Cargo.toml"), &wrapper_manifest(&set))?;
    let mut lib_rs = String::new();
    for member in &set {
        let _ = writeln!(lib_rs, "extern crate {};", crate_ident(&member.name));
    }
    write_if_different(&project.join("lib.rs"), &lib_rs)?;
    seed_lockfile(&project, TOOLCHAIN_LOCK)?;
    let target_dir = dir.join("target");
    run_cargo_build(
        &CargoBuild {
            manifest_path: &project.join("Cargo.toml"),
            target_dir: &target_dir,
            profile: Profile::Release,
            cargo_target: None,
            target_args: &["--lib"],
            crate_label: &crates.join(", "),
            rustc_args: &[],
        },
        None,
    )?;
    let output = target_dir.join("release").join(format!(
        "{}{LIBRARY_NAME}{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ));
    let staged = built
        .library
        .with_extension(format!("{}.part", std::process::id()));
    fs::copy(&output, &staged)?;
    fs::rename(&staged, &built.library)?;
    prune_other_builds(&dir, &stem);
    let _ = crate::cache_maintenance::prune_binding_root(
        &cache_root.join("bindings"),
        crate::cache_maintenance::CachePolicy::default(),
        false,
        Some(&dir),
    );
    Ok(built)
}

/// Removes the builds of earlier sources. A library a running program
/// still has loaded cannot be removed on every platform; it goes on a later
/// build.
fn prune_other_builds(dir: &Path, current: &str) {
    let Ok(entries) = fs::read_dir(dir.join("lib")) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let stem = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.split('.').next())
            .unwrap_or_default()
            .to_string();
        if stem != current {
            let _ = fs::remove_file(&path);
        }
    }
}

/// The crate Cargo builds the set's library from: it depends on each binding
/// as `[rust-bindings]` declares it, in one crate graph, and names each so
/// its registry and exports link into the library.
fn wrapper_manifest(members: &[Member]) -> String {
    let deps: Vec<&str> = members.iter().map(|m| m.dep_line.as_str()).collect();
    let deps = deps.join("\n");
    format!(
        r#"# generated by gos -- do not edit
[package]
name = "gos-bindings"
version = "0.0.0"
edition = "2024"
publish = false

# A workspace of its own, so a cache under some other workspace's tree is
# not absorbed into it.
[workspace]

[lib]
name = "{LIBRARY_NAME}"
crate-type = ["cdylib"]
path = "lib.rs"

[dependencies]
{deps}

# A binding's panic is caught at its thunk and raised as a Gossamer panic,
# which needs unwinding.
[profile.release]
opt-level = 3
lto = "thin"
panic = "unwind"
incremental = false
{release_strip}
# Build scripts and proc-macros run only on the build machine, and rustc loads
# a proc-macro through the crate metadata in its dylib. They stay unstripped:
# the Xcode 27 linker lays out a stripped macOS dylib built for a deployment
# target the SDK supplies in a way rustc cannot read that metadata from.
[profile.release.build-override]
strip = false

{patch}"#,
        patch = binding_crates_patch(members.iter().filter_map(|m| m.local_root.as_deref())),
        release_strip = RELEASE_STRIP,
    )
}

// rustc's strip of a macOS dylib leaves a LINKEDIT string pool the macOS 27
// dyld refuses to load as mis-aligned, so the library keeps its symbols there.
#[cfg(target_os = "macos")]
const RELEASE_STRIP: &str = "strip = false\n";
#[cfg(not(target_os = "macos"))]
const RELEASE_STRIP: &str = "";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_dependencies_are_read_from_every_dependency_table() {
        let dir = std::env::temp_dir().join(format!("gos-binding-deps-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            r#"[package]
name = "x"

[dependencies]
a = { path = "../a" }
b = "1"

[target.'cfg(unix)'.dependencies]
c = { path = "c" }
"#,
        )
        .unwrap();
        let deps = path_dependencies(&dir.join("Cargo.toml"));
        assert_eq!(deps, vec![dir.join("../a"), dir.join("c")]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_edit_to_a_path_binding_changes_its_build_name() {
        let dir = std::env::temp_dir().join(format!("gos-binding-stem-{}", std::process::id()));
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        fs::write(dir.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        let set = [member("x", "x = { path = '/x' }", Some(&dir))];
        let first = build_stem(Path::new("/cache/x"), &set).unwrap();
        assert_eq!(first, build_stem(Path::new("/cache/x"), &set).unwrap());
        fs::write(dir.join("src/lib.rs"), "pub fn a() { let _ = 1; }\n").unwrap();
        let second = build_stem(Path::new("/cache/x"), &set).unwrap();
        assert_ne!(first, second);
        let _ = fs::remove_dir_all(&dir);
    }

    fn member(name: &str, dep_line: &str, root: Option<&Path>) -> Member {
        Member {
            name: name.to_string(),
            dep_line: dep_line.to_string(),
            local_root: root.map(Path::to_path_buf),
            by_path: true,
        }
    }

    fn path_spec(dir: &Path) -> RustBindingSpec {
        RustBindingSpec::Path {
            version: None,
            path: dir.to_string_lossy().into_owned(),
            features: Vec::new(),
            default_features: true,
        }
    }

    #[test]
    fn a_prebuilt_library_stands_in_only_for_the_set_it_was_built_from() {
        let scratch = std::env::temp_dir().join(format!("gos-prebuilt-{}", std::process::id()));
        let mut bindings = BTreeMap::new();
        for name in ["one", "two"] {
            let dir = scratch.join(name);
            fs::create_dir_all(dir.join("src")).unwrap();
            fs::write(
                dir.join("Cargo.toml"),
                format!("[package]\nname = \"{name}\"\n"),
            )
            .unwrap();
            fs::write(dir.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
            bindings.insert(name.to_string(), path_spec(&dir));
        }
        let set = members(&bindings, &scratch);
        let key = prebuilt_key(&set).unwrap().unwrap();
        let library = prebuilt_path(&scratch.join("one"), &key);
        fs::create_dir_all(library.parent().unwrap()).unwrap();
        fs::write(&library, b"prebuilt").unwrap();

        let cache = scratch.join("cache");
        let found = ensure_bindings_library_in(&bindings, &scratch, &cache).unwrap();
        assert_eq!(fs::read(&found.library).unwrap(), b"prebuilt");

        let target = scratch.join("out");
        assert!(
            !take_prebuilt(&set[..1], &target).unwrap(),
            "a subset is another set"
        );
        fs::write(
            scratch.join("two/src/lib.rs"),
            "pub fn a() { let _ = 1; }\n",
        )
        .unwrap();
        assert!(!take_prebuilt(&set, &target).unwrap(), "edited sources");
        let _ = fs::remove_dir_all(&scratch);
    }

    #[test]
    fn the_cache_key_separates_sets_sources_and_targets() {
        let root = Path::new("/cache");
        let a = [member("x", "x = { path = '/a' }", None)];
        let b = [member("x", "x = { path = '/b' }", None)];
        let both = [a[0].clone(), member("y", "y = { path = '/y' }", None)];
        let key = |set: &[Member], target| library_dir(root, set, target);
        assert_ne!(key(&a, None), key(&b, None));
        assert_ne!(key(&a, None), key(&both, None));
        assert_ne!(key(&a, None), key(&a, Some("aarch64-unknown-linux-gnu")));
        assert_eq!(key(&a, None), key(&a, None));
        assert!(key(&both, None).to_string_lossy().contains("x+1-"));
    }
}
