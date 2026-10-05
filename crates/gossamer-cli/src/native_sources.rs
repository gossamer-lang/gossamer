//! Compiling the C and assembly sources a package's `[native]` table names.
//!
//! Each package's sources are compiled with the target's C compiler into
//! objects cached under the project's `.gos-cache/native/`, keyed on the
//! compiler, its arguments, the target, and the contents of the source and
//! of every header it included, which the compiler reports as it compiles.
//! A native build links them as one static archive per package; the
//! bytecode VM and the JIT load them as one shared library per package,
//! which foreign declarations without a `#[link]` resolve against.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow};
use gossamer_pkg::NativeSpec;
use gossamer_pkg::sha256::Hasher;
use parking_lot::Mutex;

/// A package whose manifest carries a `[native]` table.
#[derive(Debug, Clone)]
pub struct NativePackage {
    /// A file-name-safe spelling of the package id's last segment.
    name: String,
    /// The directory holding the package's `project.toml`.
    root: PathBuf,
    spec: NativeSpec,
}

/// The `[native]` packages of the program rooted at `entry`: its own
/// project's and every dependency's.
pub fn packages(entry: &Path) -> Result<Vec<NativePackage>> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let entry = cwd.join(entry);
    let mut roots: Vec<(String, PathBuf)> = Vec::new();
    if let Some(root) =
        gossamer_pkg::find_manifest(&entry).and_then(|m| m.parent().map(Path::to_path_buf))
    {
        roots.push((String::new(), root));
    }
    roots.extend(gossamer_pkg::bundle::path_dependency_roots(&entry));
    let mut found = Vec::new();
    for (package, root) in roots {
        let manifest_path = root.join("project.toml");
        let Ok(text) = fs::read_to_string(&manifest_path) else {
            continue;
        };
        let manifest = gossamer_pkg::Manifest::parse(&text)
            .with_context(|| format!("parsing {}", manifest_path.display()))?;
        let Some(spec) = manifest.native else {
            continue;
        };
        if let Some(feature) = &spec.feature
            && !gossamer_ast::cfg::feature_enabled(&package, feature)
        {
            continue;
        }
        let id = manifest.project.id.to_string();
        let leaf = id.rsplit('/').next().unwrap_or(&id);
        let name: String = leaf
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        found.push(NativePackage { name, root, spec });
    }
    Ok(found)
}

/// A static archive a native link names as a library.
#[derive(Debug, Clone)]
pub struct NativeArchive {
    /// The directory holding it, searched by the link.
    pub dir: PathBuf,
    /// The library name the link line spells: `gosnative_<package>`.
    pub name: String,
    /// The archive itself.
    pub path: PathBuf,
}

/// What a native build links from the `[native]` packages, and the files
/// it read to make them.
#[derive(Debug, Clone, Default)]
pub struct NativeBuild {
    /// One archive per package.
    pub archives: Vec<NativeArchive>,
    /// Every manifest, source, and included header the archives were made
    /// from: a later build is current only while these are unchanged.
    pub inputs: Vec<PathBuf>,
}

/// The static archives a native build for `target` (the host when `None`)
/// links: one per `[native]` package of the program rooted at `entry`.
pub fn archives(entry: &Path, target: Option<&str>) -> Result<NativeBuild> {
    let packages = packages(entry)?;
    let mut build = NativeBuild::default();
    if packages.is_empty() {
        return Ok(build);
    }
    let toolchain = Toolchain::for_target(target)?;
    let cache = cache_root(entry).join(&toolchain.target);
    for package in &packages {
        let dir = cache.join(&package.name);
        let objects = compile(package, &toolchain, &dir)?;
        let archive = toolchain.archive(&package.name, &objects, &dir.join("static"))?;
        build.inputs.push(package.root.join("project.toml"));
        for object in &objects {
            build
                .inputs
                .extend(recorded_inputs(&object.with_extension("deps")));
        }
        build.archives.push(NativeArchive {
            dir: archive.parent().map(Path::to_path_buf).unwrap_or_default(),
            name: format!("gosnative_{}", package.name),
            path: archive,
        });
    }
    build.inputs.sort();
    build.inputs.dedup();
    Ok(build)
}

/// The files a dependency record lists.
fn recorded_inputs(record: &Path) -> Vec<PathBuf> {
    fs::read_to_string(record)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.split_once('\t').map(|(_, path)| PathBuf::from(path)))
        .collect()
}

/// The shared libraries the bytecode VM and the JIT load for the program
/// rooted at `entry`: one per `[native]` package, built for the host.
/// `libraries` and `search` are the program's own `#[link]` libraries,
/// which a shim may call into and which a Windows DLL resolves at link.
pub fn shared_libraries(
    entry: &Path,
    libraries: &[String],
    search: &[PathBuf],
) -> Result<Vec<PathBuf>> {
    let packages = packages(entry)?;
    if packages.is_empty() {
        return Ok(Vec::new());
    }
    let toolchain = Toolchain::for_target(None)?;
    let cache = cache_root(entry).join(&toolchain.target);
    packages
        .iter()
        .map(|package| {
            let dir = cache.join(&package.name);
            let objects = compile(package, &toolchain, &dir)?;
            toolchain.shared(
                &package.name,
                &objects,
                &dir.join("shared"),
                libraries,
                search,
            )
        })
        .collect()
}

/// The host compiler `[native]` sources compile with, and what it reports
/// itself to be, for `gos env`.
#[must_use]
pub fn compiler_description() -> String {
    match Toolchain::for_target(None) {
        Ok(toolchain) => toolchain.identity.replace('\n', " - "),
        Err(err) => format!("<not found: {err}>"),
    }
}

/// `.gos-cache/native` beside the project's manifest, or beside `entry`.
fn cache_root(entry: &Path) -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let entry = cwd.join(entry);
    let base = gossamer_pkg::find_manifest(&entry)
        .and_then(|manifest| manifest.parent().map(Path::to_path_buf))
        .or_else(|| entry.parent().map(Path::to_path_buf))
        .unwrap_or(cwd);
    base.join(".gos-cache").join("native")
}

/// The C compiler and archiver for one target.
struct Toolchain {
    tool: cc::Tool,
    archiver: PathBuf,
    target: String,
    /// The compiler's own report of what it is, part of every object's key.
    identity: String,
    msvc: bool,
}

impl Toolchain {
    /// The toolchain for `target`, the host when `None`: `GOS_CC` when set,
    /// otherwise the compiler the platform's conventions name (`CC`, then
    /// `cc` or the MSVC `cl` an installed Visual Studio provides).
    fn for_target(target: Option<&str>) -> Result<Self> {
        let host = gossamer_driver::TargetTriple::host().as_str().to_string();
        let target = target.unwrap_or(&host).to_string();
        let mut build = cc::Build::new();
        build
            .target(&target)
            .host(&host)
            .opt_level(2)
            .debug(false)
            .pic(true)
            .warnings(false)
            .cargo_metadata(false)
            .cargo_warnings(false);
        if let Some(compiler) = std::env::var_os("GOS_CC") {
            build.compiler(compiler);
        }
        let tool = build
            .try_get_compiler()
            .map_err(|err| anyhow!("no C compiler for `{target}`: {err}"))?;
        let archiver = build
            .try_get_archiver()
            .map_err(|err| anyhow!("no archiver for `{target}`: {err}"))?;
        let archiver = PathBuf::from(archiver.get_program());
        let msvc = tool.is_like_msvc();
        let identity = compiler_identity(tool.path(), msvc);
        Ok(Self {
            tool,
            archiver,
            target,
            identity,
            msvc,
        })
    }

    fn object_extension(&self) -> &'static str {
        if self.msvc { "obj" } else { "o" }
    }

    /// The arguments every source of `package` is compiled with, beyond
    /// the compiler's own target defaults.
    fn package_args(&self, package: &NativePackage) -> Vec<String> {
        let mut args = Vec::new();
        for dir in &package.spec.include {
            let dir = package.root.join(dir);
            args.push(if self.msvc {
                format!("/I{}", dir.display())
            } else {
                format!("-I{}", dir.display())
            });
        }
        let flag = if self.msvc { "/D" } else { "-D" };
        for (name, value) in &package.spec.defines {
            args.push(if value.is_empty() {
                format!("{flag}{name}")
            } else {
                format!("{flag}{name}={value}")
            });
        }
        args.extend(package.spec.flags.iter().cloned());
        args
    }

    /// Writes `dir/lib<name>.a` (`<name>.lib` with MSVC) from `objects`,
    /// unless an archive of exactly these objects is already there.
    fn archive(&self, name: &str, objects: &[PathBuf], dir: &Path) -> Result<PathBuf> {
        let file = if self.msvc {
            format!("gosnative_{name}.lib")
        } else {
            format!("libgosnative_{name}.a")
        };
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let out = dir.join(file);
        if product_current(&out, objects) {
            return Ok(out);
        }
        let _ = fs::remove_file(&out);
        let mut cmd = Command::new(&self.archiver);
        if self.msvc {
            cmd.arg("/NOLOGO").arg(format!("/OUT:{}", out.display()));
        } else {
            cmd.arg("crs").arg(&out);
        }
        cmd.args(objects);
        run(cmd, "archiving the native objects")?;
        record_product(&out, objects)?;
        Ok(out)
    }

    /// Writes the shared library the VM loads from `objects`, unless one
    /// of exactly these objects is already there.
    fn shared(
        &self,
        name: &str,
        objects: &[PathBuf],
        dir: &Path,
        libraries: &[String],
        search: &[PathBuf],
    ) -> Result<PathBuf> {
        let file = if cfg!(windows) {
            format!("gosnative_{name}.dll")
        } else if cfg!(target_os = "macos") {
            format!("libgosnative_{name}.dylib")
        } else {
            format!("libgosnative_{name}.so")
        };
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let out = dir.join(file);
        if product_current(&out, objects) {
            return Ok(out);
        }
        let _ = fs::remove_file(&out);
        let mut cmd = self.tool.to_command();
        if self.msvc {
            // A DLL exports only what its module definition lists: every
            // function the objects define.
            let def = dir.join(format!("gosnative_{name}.def"));
            fs::write(&def, module_definition(objects)?)
                .with_context(|| format!("writing {}", def.display()))?;
            cmd.arg("/nologo").arg("/LD").args(objects);
            cmd.arg(format!("/Fe:{}", out.display()));
            cmd.arg("/link").arg(format!("/DEF:{}", def.display()));
            for dir in search {
                cmd.arg(format!("/LIBPATH:{}", dir.display()));
            }
            for library in libraries {
                cmd.arg(format!("{library}.lib"));
            }
        } else {
            cmd.arg("-shared").args(objects).arg("-o").arg(&out);
            if cfg!(target_os = "macos") {
                // Symbols a shim takes from the program's other libraries
                // resolve when the VM loads it, as they do on Linux.
                cmd.arg("-undefined").arg("dynamic_lookup");
            }
            if cfg!(windows) {
                cmd.arg("-Wl,--export-all-symbols");
            }
            for dir in search {
                cmd.arg(format!("-L{}", dir.display()));
            }
            for library in libraries {
                cmd.arg(format!("-l{library}"));
            }
        }
        run(cmd, "linking the native shared library")?;
        record_product(&out, objects)?;
        Ok(out)
    }
}

/// What `compiler` reports itself to be: its `--version` banner, or with
/// MSVC the banner `cl` prints with no arguments. Asked once per compiler.
fn compiler_identity(compiler: &Path, msvc: bool) -> String {
    static SEEN: OnceLock<Mutex<BTreeMap<PathBuf, String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(BTreeMap::new()));
    if let Some(identity) = seen.lock().get(compiler) {
        return identity.clone();
    }
    let mut cmd = Command::new(compiler);
    if !msvc {
        cmd.arg("--version");
    }
    let identity = cmd.output().map_or_else(
        |_| compiler.display().to_string(),
        |out| {
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            format!(
                "{}\n{}",
                compiler.display(),
                text.lines().next().unwrap_or("")
            )
        },
    );
    seen.lock().insert(compiler.to_path_buf(), identity.clone());
    identity
}

fn sha256_hex(parts: &[&[u8]]) -> String {
    let mut hasher = Hasher::new();
    for part in parts {
        hasher.update(&(part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    hasher.finalize_hex()
}

fn file_hash(path: &Path) -> Option<String> {
    fs::read(path).ok().map(|bytes| sha256_hex(&[&bytes]))
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

/// The objects of `package`'s sources, compiled into `dir` where a cached
/// object is not still current.
fn compile(package: &NativePackage, toolchain: &Toolchain, dir: &Path) -> Result<Vec<PathBuf>> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let package_args = package_args_line(toolchain, package);
    let mut objects = Vec::with_capacity(package.spec.sources.len());
    for source in &package.spec.sources {
        let path = package.root.join(source);
        let content = fs::read(&path).with_context(|| {
            format!(
                "reading the [native] source `{source}` of {}",
                package.root.join("project.toml").display()
            )
        })?;
        let is_assembly = Path::new(source)
            .extension()
            .is_some_and(|ext| ext == "s" || ext == "S");
        if is_assembly && toolchain.msvc {
            return Err(anyhow!(
                "the [native] source `{source}` is assembly, which MSVC's `cl` does not \
                 compile; set GOS_CC to clang or gcc"
            ));
        }
        let key = sha256_hex(&[
            toolchain.identity.as_bytes(),
            toolchain.target.as_bytes(),
            package_args.as_bytes(),
            path.to_string_lossy().as_bytes(),
            &content,
        ]);
        let object = dir.join(format!("{key}.{}", toolchain.object_extension()));
        let deps = dir.join(format!("{key}.deps"));
        if object.is_file() && dependencies_current(&deps) {
            objects.push(object);
            continue;
        }
        let included = compile_one(toolchain, package, &path, &object)?;
        let mut record = String::new();
        for dependency in std::iter::once(path.clone()).chain(included) {
            if let Some(hash) = file_hash(&dependency) {
                record.push_str(&format!("{hash}\t{}\n", dependency.display()));
            }
        }
        fs::write(&deps, record).with_context(|| format!("writing {}", deps.display()))?;
        objects.push(object);
    }
    Ok(objects)
}

fn package_args_line(toolchain: &Toolchain, package: &NativePackage) -> String {
    let mut line: Vec<String> = toolchain
        .tool
        .args()
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    line.extend(toolchain.package_args(package));
    line.join("\u{1f}")
}

/// Compiles `source` to `object`, answering the headers it included.
fn compile_one(
    toolchain: &Toolchain,
    package: &NativePackage,
    source: &Path,
    object: &Path,
) -> Result<Vec<PathBuf>> {
    let mut cmd = toolchain.tool.to_command();
    cmd.args(toolchain.package_args(package));
    let what = format!("compiling the [native] source {}", source.display());
    if toolchain.msvc {
        cmd.arg("/nologo").arg("/c").arg(source);
        cmd.arg(format!("/Fo:{}", object.display()));
        cmd.arg("/showIncludes");
        let out = cmd.output().with_context(|| what.clone())?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        if !out.status.success() {
            return Err(anyhow!(
                "{what} failed:\n{stdout}{}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        // `cl /showIncludes` reports each header on its own line.
        Ok(stdout
            .lines()
            .filter_map(|line| line.split_once("including file:"))
            .map(|(_, path)| PathBuf::from(path.trim()))
            .collect())
    } else {
        let depfile = object.with_extension("d");
        cmd.arg("-c").arg(source).arg("-o").arg(object);
        cmd.arg("-MD").arg("-MF").arg(&depfile);
        run(cmd, &what)?;
        let text = fs::read_to_string(&depfile).unwrap_or_default();
        let _ = fs::remove_file(&depfile);
        Ok(depfile_inputs(&text))
    }
}

/// The inputs a make-style dependency file lists after its target.
fn depfile_inputs(text: &str) -> Vec<PathBuf> {
    let joined = text.replace("\\\r\n", " ").replace("\\\n", " ");
    // The target ends at the first `: ` - a Windows drive letter's colon is
    // followed by a path separator instead.
    let Some((_, inputs)) = joined.split_once(": ") else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    let mut current = String::new();
    let mut chars = inputs.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&' ') => {
                current.push(' ');
                chars.next();
            }
            c if c.is_whitespace() => {
                if !current.is_empty() {
                    paths.push(PathBuf::from(std::mem::take(&mut current)));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        paths.push(PathBuf::from(current));
    }
    paths
}

/// Whether every file a cached object was built from still holds what it
/// held then.
fn dependencies_current(record: &Path) -> bool {
    let Ok(text) = fs::read_to_string(record) else {
        return false;
    };
    text.lines().all(|line| {
        line.split_once('\t')
            .is_some_and(|(hash, path)| file_hash(Path::new(path)).as_deref() == Some(hash))
    })
}

/// The record beside a product naming the objects it was made from.
fn product_record(product: &Path) -> PathBuf {
    let mut name = product.as_os_str().to_owned();
    name.push(".objects");
    PathBuf::from(name)
}

/// The objects a product is made from, by path and contents: an object
/// rebuilt in place for a changed header is a different input.
fn objects_line(objects: &[PathBuf]) -> String {
    objects
        .iter()
        .map(|object| {
            let hash = file_hash(object).unwrap_or_default();
            format!("{hash}\t{}", object.display())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn product_current(product: &Path, objects: &[PathBuf]) -> bool {
    product.is_file()
        && fs::read_to_string(product_record(product))
            .is_ok_and(|text| text == objects_line(objects))
}

fn record_product(product: &Path, objects: &[PathBuf]) -> Result<()> {
    let record = product_record(product);
    fs::write(&record, objects_line(objects))
        .with_context(|| format!("writing {}", record.display()))
}

/// A module definition exporting every function `objects` define.
fn module_definition(objects: &[PathBuf]) -> Result<String> {
    use object::{Object, ObjectSymbol, SymbolKind};

    let mut text = String::from("EXPORTS\n");
    for path in objects {
        let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let file = object::File::parse(bytes.as_slice())
            .map_err(|err| anyhow!("reading the symbols of {}: {err}", path.display()))?;
        for symbol in file.symbols() {
            if symbol.is_global() && symbol.is_definition() && symbol.kind() == SymbolKind::Text {
                if let Ok(name) = symbol.name() {
                    text.push_str(&format!("    {name}\n"));
                }
            }
        }
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depfile_inputs_follow_continuations_and_escaped_spaces() {
        let text = "obj.o: src/a.c include/a.h \\\n  include/my\\ dir/b.h\n";
        assert_eq!(
            depfile_inputs(text),
            vec![
                PathBuf::from("src/a.c"),
                PathBuf::from("include/a.h"),
                PathBuf::from("include/my dir/b.h"),
            ]
        );
    }

    #[test]
    fn depfile_inputs_keep_a_drive_letter_in_the_target() {
        let text = "C:\\build\\obj.o: C:\\src\\a.c\n";
        assert_eq!(depfile_inputs(text), vec![PathBuf::from("C:\\src\\a.c")]);
    }
}
