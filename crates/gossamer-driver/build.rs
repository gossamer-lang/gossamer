//! Build script: emits a per-compile stamp folded into the frontend-cache key,
//! and packs the runtime and binding crate sources this toolchain links
//! against into `$OUT_DIR/toolchain-src.bin`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// The crates a generated binding project compiles from this toolchain's
/// sources; the snapshot also carries every workspace crate they reach.
const SNAPSHOT_ROOTS: [&str; 4] = [
    "gossamer-runtime",
    "gossamer-binding",
    "gossamer-binding-abi",
    "gossamer-binding-macros",
];

fn main() {
    // The stamp must change whenever FRONTEND behavior changes, not just
    // this crate: watch every crate whose code shapes the cached AST.
    for dir in [
        "../gossamer-lex/src",
        "../gossamer-ast/src",
        "../gossamer-parse/src",
        "../gossamer-resolve/src",
        "../gossamer-hir/src",
        "src",
    ] {
        println!("cargo:rerun-if-changed={dir}");
    }
    // Per-compile stamp folded into the frontend-cache key: the
    // declared crate version alone is constant across development
    // rebuilds, so cached ASTs parsed by an older compiler were
    // served as fresh (stale parse-time rewrites, stale autoderive).
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos().to_string())
        .unwrap_or_default();
    println!("cargo:rustc-env=GOS_DRIVER_BUILD_STAMP={stamp}");

    pack_toolchain_sources();
}

/// Writes the source snapshot and exports its digest as
/// `GOS_TOOLCHAIN_SOURCE_DIGEST`.
///
/// The snapshot is a deflated sequence of `(path, contents)` records in path
/// order, so the same sources always pack to the same bytes and digest.
fn pack_toolchain_sources() {
    let manifest_dir = PathBuf::from(env("CARGO_MANIFEST_DIR"));
    let workspace = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("gossamer-driver sits two levels below the workspace root")
        .to_path_buf();
    let root_manifest = workspace.join("Cargo.toml");
    println!("cargo:rerun-if-changed={}", root_manifest.display());
    let root: toml::Table = toml::from_str(&read(&root_manifest)).expect("workspace Cargo.toml");
    let workspace_table = root
        .get("workspace")
        .and_then(toml::Value::as_table)
        .expect("the root manifest declares a workspace");
    let workspace_deps = workspace_table
        .get("dependencies")
        .and_then(toml::Value::as_table)
        .cloned()
        .unwrap_or_default();

    let crates = crate_closure(&workspace, &workspace_deps);
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for dir in crates.values() {
        let relative = dir
            .strip_prefix(&workspace)
            .expect("snapshot crates live in the workspace");
        for name in ["Cargo.toml", "build.rs"] {
            let path = dir.join(name);
            if path.is_file() {
                println!("cargo:rerun-if-changed={}", path.display());
                files.insert(slash_path(&relative.join(name)), read_bytes(&path));
            }
        }
        let src = dir.join("src");
        println!("cargo:rerun-if-changed={}", src.display());
        let mut sources = Vec::new();
        collect_files(&src, &mut sources);
        for path in sources {
            let rel = path
                .strip_prefix(&workspace)
                .expect("crate sources live in the workspace");
            files.insert(slash_path(rel), read_bytes(&path));
        }
    }
    files.insert(
        "Cargo.toml".to_string(),
        snapshot_manifest(workspace_table, &workspace, &crates).into_bytes(),
    );

    let mut payload = Vec::new();
    for (path, contents) in &files {
        payload.extend_from_slice(&(path.len() as u32).to_le_bytes());
        payload.extend_from_slice(path.as_bytes());
        payload.extend_from_slice(&(contents.len() as u64).to_le_bytes());
        payload.extend_from_slice(contents);
    }
    let digest = Sha256::digest(&payload);
    let digest_hex = digest.iter().fold(String::new(), |mut hex, b| {
        let _ = write!(hex, "{b:02x}");
        hex
    });
    let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(&payload).expect("deflate into memory");
    let packed = encoder.finish().expect("deflate into memory");
    let out = PathBuf::from(env("OUT_DIR")).join("toolchain-src.bin");
    fs::write(&out, packed).expect("write the toolchain source snapshot");
    println!("cargo:rustc-env=GOS_TOOLCHAIN_SOURCE_DIGEST={digest_hex}");
}

/// Every workspace crate the snapshot roots reach through their normal and
/// build dependencies, by name.
fn crate_closure(workspace: &Path, workspace_deps: &toml::Table) -> BTreeMap<String, PathBuf> {
    let mut found: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut pending: Vec<String> = SNAPSHOT_ROOTS.iter().map(|s| (*s).to_string()).collect();
    while let Some(name) = pending.pop() {
        if found.contains_key(&name) {
            continue;
        }
        let Some(dir) = workspace_deps
            .get(&name)
            .and_then(|spec| spec.get("path"))
            .and_then(toml::Value::as_str)
            .map(|path| workspace.join(path))
        else {
            panic!("snapshot crate `{name}` has no path in [workspace.dependencies]");
        };
        let manifest: toml::Table =
            toml::from_str(&read(&dir.join("Cargo.toml"))).expect("crate Cargo.toml");
        let mut tables: Vec<&toml::Table> = Vec::new();
        for key in ["dependencies", "build-dependencies"] {
            if let Some(table) = manifest.get(key).and_then(toml::Value::as_table) {
                tables.push(table);
            }
        }
        if let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) {
            for target in targets.values() {
                for key in ["dependencies", "build-dependencies"] {
                    if let Some(table) = target.get(key).and_then(toml::Value::as_table) {
                        tables.push(table);
                    }
                }
            }
        }
        for table in tables {
            for (dep, spec) in table {
                let local = spec.get("path").is_some()
                    || (spec.get("workspace").and_then(toml::Value::as_bool) == Some(true)
                        && workspace_deps
                            .get(dep)
                            .is_some_and(|ws| ws.get("path").is_some()));
                if local {
                    pending.push(dep.clone());
                }
            }
        }
        found.insert(name, dir);
    }
    found
}

/// The snapshot's root manifest: this workspace's shared package fields,
/// lints and dependency table, with only the snapshot crates as members.
fn snapshot_manifest(
    workspace_table: &toml::Table,
    workspace: &Path,
    crates: &BTreeMap<String, PathBuf>,
) -> String {
    let mut table = toml::Table::new();
    for key in ["resolver", "package", "lints", "dependencies"] {
        if let Some(value) = workspace_table.get(key) {
            table.insert(key.to_string(), value.clone());
        }
    }
    let members: Vec<toml::Value> = crates
        .values()
        .map(|dir| {
            toml::Value::String(slash_path(
                dir.strip_prefix(workspace)
                    .expect("snapshot crates live in the workspace"),
            ))
        })
        .collect();
    table.insert("members".to_string(), toml::Value::Array(members));
    let mut root = toml::Table::new();
    root.insert("workspace".to_string(), toml::Value::Table(table));
    toml::to_string(&root).expect("serialize the snapshot manifest")
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|err| panic!("read {}: {err}", dir.display()))
        .map(|entry| entry.expect("directory entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect_files(&path, out);
        } else if path.is_file() {
            out.push(path);
        }
    }
}

/// `path` with `/` separators, as the snapshot records it on every host.
fn slash_path(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("cargo sets {name}"))
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
}

fn read_bytes(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()))
}
