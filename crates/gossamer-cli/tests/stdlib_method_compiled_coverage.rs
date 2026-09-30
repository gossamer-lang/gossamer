//! Compiled-tier coverage gate for the stdlib's handle-type method surface.
//!
//! The free-function twin of this gate lives in `stdlib_compiled_coverage.rs`.
//! Methods need their own because they reach the bytecode VM by a different
//! route: `Vm::lookup_builtin_method` falls back to the prelude keyed by the
//! method's BARE name, so a handle method bound only as an interp builtin
//! dispatches fine under `gos` and `gos test` - which runs the whole suite on
//! pure bytecode - and only fails at `gos build` with
//! `undefined symbols before LLVM tools: @<name> referenced from gos_main`.
//!
//! The advertised surface is [`HANDLE_SIGNATURES`], the catalogue `%info` and
//! `%explain` answer from. A row whose signature takes `self:` is an instance
//! method and must reach a bare-name arm in one of the MIR method-dispatch
//! tables; every other row is an associated function and must reach a
//! `Type::name`-keyed arm, exactly as a free function does.
//!
//! Adding a handle method without wiring its compiled lowering makes this test
//! fail with the exact `Type::method` pair.

#![allow(missing_docs)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use gossamer_cli::repl_handles::HANDLE_SIGNATURES;

/// MIR lowering files, and module directories, whose `match` arms key on a
/// method name or on a `Type::name` path. Their string-literal patterns are
/// the enumerable set of compiled-lowerable method spellings.
const DISPATCH_SOURCES: &[&str] = &[
    "crates/gossamer-mir/src/lower/builder/method_call.rs",
    "crates/gossamer-mir/src/lower/builder/method_call",
    "crates/gossamer-mir/src/lower/builder/method_call_dispatch.rs",
    "crates/gossamer-mir/src/lower/builder/expr_call.rs",
    "crates/gossamer-mir/src/lower/builder/intrinsic.rs",
    "crates/gossamer-mir/src/lower/builder/intrinsic",
    "crates/gossamer-mir/src/lower/builder/stdlib.rs",
    "crates/gossamer-mir/src/lower/builder/stdlib_binding.rs",
    "crates/gossamer-mir/src/lower/builder/stdlib_free.rs",
    "crates/gossamer-mir/src/lower/builder/stdlib_free",
];

/// `Type::method` pairs the compiled tiers reach through a mechanism other
/// than a name-keyed dispatch arm. Each entry names the mechanism and has been
/// verified to survive `gos build`. This is a closed list: a method that
/// simply has no compiled lowering does not belong here - wire the lowering.
const COMPILED_VIA_SPECIAL_MECHANISM: &[(&str, &str, &str)] = &[
    (
        "Value",
        "object",
        "the whole json module is recognised by its segment chain in \
         lower_json_free_call, not by per-function patterns",
    ),
    (
        "Http2Config",
        "default",
        "an autoderive stdlib wrapper: the parse-time rewrite folds the call \
         into the injected `__gos_http_Http2Config_default`, a real Gossamer \
         function every tier compiles",
    ),
    (
        "Channel",
        "new",
        "the channel constructor lowers through the `sync::channel` \
         intrinsic, which builds the sender/receiver pair rather than \
         dispatching a name",
    ),
    (
        "I64Vec",
        "new",
        "lowered by the heap-vec constructor path, which keys on the \
         destination's element repr rather than the type path",
    ),
    (
        "U8Vec",
        "new",
        "lowered by the heap-vec constructor path, which keys on the \
         destination's element repr rather than the type path",
    ),
    (
        "Mutex",
        "new",
        "lowered by the sync-handle constructor path, which keys on the \
         guarded value's repr rather than the type path",
    ),
    (
        "WaitGroup",
        "new",
        "lowered by the sync-handle constructor path, which keys on the \
         guarded value's repr rather than the type path",
    ),
];

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf()
}

/// Splits a dispatch source's string literals into bare method names and
/// `::`-joined paths. Runtime symbols (`gos_rt_*`) and compiler-synthesised
/// names (`__gos*`) are never dispatch keys and are excluded by construction.
fn dispatch_literals(src: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut bare = BTreeSet::new();
    let mut paths = BTreeSet::new();
    for (index, piece) in src.split('"').enumerate() {
        // Odd indices are the contents of a string literal.
        if index % 2 == 0 || piece.is_empty() {
            continue;
        }
        if piece.starts_with("gos_rt") || piece.starts_with("__gos") {
            continue;
        }
        if piece.contains("::") {
            if piece
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
            {
                paths.insert(piece.to_string());
            }
            continue;
        }
        let identifier = piece.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
            && piece
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if identifier {
            bare.insert(piece.to_string());
        }
    }
    (bare, paths)
}

/// Whether an associated function `Type::name` is keyed by some dispatch arm.
/// The owner is written without its module prefix in the catalogue, so a
/// module-qualified arm (`sync::AtomicI64::new`) matches on its tail.
fn associated_fn_reachable(owner: &str, name: &str, paths: &BTreeSet<String>) -> bool {
    let tail = format!("{owner}::{name}");
    paths
        .iter()
        .any(|path| path == &tail || path.ends_with(&format!("::{tail}")))
}

/// The text of every `.rs` file a dispatch source names: the file itself, or
/// each file under it when it is a module directory.
fn source_texts(root: &std::path::Path, rel: &str) -> Vec<String> {
    let path = root.join(rel);
    if path.is_file() {
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        return vec![text];
    }
    let mut texts = Vec::new();
    let mut pending = vec![path];
    while let Some(dir) = pending.pop() {
        let entries =
            std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
        for entry in entries {
            let entry = entry.expect("a directory entry").path();
            if entry.is_dir() {
                pending.push(entry);
            } else if entry.extension().and_then(|e| e.to_str()) == Some("rs") {
                texts.push(
                    std::fs::read_to_string(&entry)
                        .unwrap_or_else(|e| panic!("read {}: {e}", entry.display())),
                );
            }
        }
    }
    texts
}

#[test]
fn stdlib_method_compiled_coverage() {
    let root = workspace_root();
    let mut bare = BTreeSet::new();
    let mut paths = BTreeSet::new();
    for rel in DISPATCH_SOURCES {
        for src in source_texts(&root, rel) {
            let (file_bare, file_paths) = dispatch_literals(&src);
            bare.extend(file_bare);
            paths.extend(file_paths);
        }
    }
    let excused: BTreeSet<(&str, &str)> = COMPILED_VIA_SPECIAL_MECHANISM
        .iter()
        .map(|(owner, name, _)| (*owner, *name))
        .collect();

    let mut vm_only: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (owner, name, signature) in HANDLE_SIGNATURES {
        if excused.contains(&(*owner, *name)) {
            continue;
        }
        let reachable = if signature.contains("self:") {
            bare.contains(*name)
        } else {
            associated_fn_reachable(owner, name, &paths) || bare.contains(*name)
        };
        if !reachable {
            vm_only.entry(name).or_default().insert(owner);
        }
    }

    let report: Vec<String> = vm_only
        .iter()
        .map(|(name, owners)| {
            format!(
                "{name} (on {})",
                owners.iter().copied().collect::<Vec<_>>().join(", ")
            )
        })
        .collect();
    assert!(
        report.is_empty(),
        "{n} stdlib handle method(s) are advertised and dispatch on the bytecode \
         VM but reach no compiled-tier lowering. `gos check`, `gos run`, and \
         `gos test` all pass (the test runner disables the JIT); `gos build` \
         fails with `undefined symbols before LLVM tools: @<name> referenced \
         from gos_main`.\nWire the MIR dispatch (a bare-name arm in \
         method_call.rs / method_call_dispatch.rs for an instance method, a \
         `Type::name` arm for an associated function) plus the rt! registry \
         row, the gos_rt_* shim, and the symbol-table entry - or, if the method \
         is reached by a non-pattern mechanism, document it in \
         COMPILED_VIA_SPECIAL_MECHANISM:\n  {}",
        report.join("\n  "),
        n = report.len(),
    );

    // Guard against the allowlist rotting: an entry naming a method the
    // catalogue no longer advertises is dead weight that would mask a
    // future regression.
    let advertised: BTreeSet<(&str, &str)> = HANDLE_SIGNATURES
        .iter()
        .map(|(owner, name, _)| (*owner, *name))
        .collect();
    let stale: Vec<String> = COMPILED_VIA_SPECIAL_MECHANISM
        .iter()
        .filter(|(owner, name, _)| !advertised.contains(&(*owner, *name)))
        .map(|(owner, name, _)| format!("{owner}::{name}"))
        .collect();
    assert!(
        stale.is_empty(),
        "COMPILED_VIA_SPECIAL_MECHANISM entries no longer advertised in \
         HANDLE_SIGNATURES (remove them): {stale:?}"
    );
}
