//! Build script for `gossamer-cli`.
//!
//! Three responsibilities:
//!
//! 1. Ensures the `gossamer-runtime` static library is present at
//!    the standard `target/<profile>/` location every time `cargo
//!    build` processes the cli, and exposes that absolute path to
//!    the cli at compile time via `GOSSAMER_RUNTIME_LIB_PATH`.
//! 2. On Linux hosts where the `x86_64-unknown-linux-musl` rustup
//!    target is installed, additionally builds the runtime
//!    against that target and exposes the resulting archive path
//!    via `GOSSAMER_RUNTIME_LIB_PATH_MUSL`. The cli's `gos build
//!    --release` link path uses that archive to produce a fully
//!    static binary (no glibc/libgcc_s/ld-linux dependency).
//! 3. Enforces dispatch-table parity: every `gos_rt_*` symbol
//!    declared in `crates/gossamer-runtime/src/c_abi.rs` must be
//!    referenced by at least one of the LLVM lowerer, the
//!    Cranelift native backend, the Cranelift JIT symbol map, or
//!    the in-file `KNOWN_UNUSED_RUNTIME_SYMBOLS` allowlist. Catches
//!    the silent-zero footgun documented in the H8 audit finding:
//!    a new runtime symbol added without wiring would otherwise
//!    compile clean but produce wrong code at run time.
//!
//! Why responsibility 1 exists: cargo only emits the `staticlib`
//! artefact when the runtime crate is the *direct* build target.
//! When the cli (or its dependents) pulls the runtime in
//! transitively as an `rlib`, the staticlib is never written. CI
//! runs that built the cli first then ran the tests would observe
//! `libgossamer_runtime.a` missing from `target/debug/`. This
//! script sidesteps that by invoking cargo against the runtime
//! crate explicitly.

use std::collections::BTreeSet;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "../gossamer-driver/src/macos_deployment.rs"]
#[allow(
    unreachable_pub,
    reason = "a build-script module the script alone consumes"
)]
mod macos_deployment;

/// Runtime symbols that intentionally have no codegen dispatch arm.
/// Add a one-line comment justifying each entry.
const KNOWN_UNUSED_RUNTIME_SYMBOLS: &[&str] = &[
    // Reached only from inside the runtime: a vector duplicating a slot that
    // holds a JSON handle takes a box of its own onto the same document.
    "gos_rt_json_clone_handle",
    // Intentionally never called from generated code: a debug-only
    // helper used by manual `gdb`/`lldb` sessions.
    "gos_rt_result_dbg",
    // Strong-count probe used by the in-process JIT trampoline
    // (`gossamer-interp`'s `free_native_enum`) to reclaim a uniquely-owned
    // returned enum DOM fully; called from Rust, never emitted by codegen.
    "gos_rt_rc_strong_count",
    // Non-buffered strong release used by `gossamer-interp`'s native-enum tree
    // teardown (`free_exclusive_enum_tree`) to reclaim an exclusively-owned tree
    // without deferring to the cycle collector; called from Rust, never emitted
    // by codegen.
    "gos_rt_rc_release_no_buffer",
    // Setup function called directly from Rust (gossamer-interp's
    // `set_runtime_program_name`), not from generated Gossamer code.
    "gos_rt_set_program_name",
    // ABI 0.4 compiled-tier callback dispatcher fallback. Referenced
    // directly from `gossamer-binding::native::NativeCallback::invoke_raw`
    // (Rust binding code), not from generated Gossamer programs. The
    // stub exists so MSVC link succeeds when the codegen hasn't
    // emitted a per-program override; Linux ld is permissive about
    // such unresolved externs but Windows isn't.
    "gos_rt_callback_invoke",
    // Cross-crate context-cancellation hooks. Installed by
    // `gossamer-std::context` at first use and called from Rust
    // callers; the LLVM tier reaches the cancellation-aware receive
    // through `gos_rt_chan_recv_ctx` instead, so this two-word
    // return shape has no generated reference of its own.
    "gos_rt_chan_recv_ctx_option",
    "gos_rt_install_ctx_hooks",
    // Installed by the bytecode VM so a fault raised in JIT-compiled code
    // reports the interpreted frames that reached it; no generated code
    // references it.
    "gos_rt_install_trace_hook",
    // Branch-coverage hooks. Codegen for `gos test --coverage` is
    // staged but not yet emitting bump/record calls - the runtime
    // surface ships ahead so the harness can install the global
    // table at startup. Wired through codegen in a follow-up patch.
    "gos_rt_cov_bump",
    "gos_rt_cov_record",
    "gos_rt_cov_reset",
    "gos_rt_cov_set_enabled",
    // Goroutine join-handle primitives. The runtime surface ships
    // ahead (spawn + join) so the interpreter can use them; the
    // compiled-tier dispatch tables (MIR → LLVM/Cranelift) are not
    // yet wired. Will be lowered once the `spawn` prelude binding
    // reaches the codegen backends.
    "gos_rt_join",
    "gos_rt_spawn",
];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../gossamer-runtime/src");
    println!("cargo:rerun-if-changed=../gossamer-runtime/Cargo.toml");
    // The workspace manifest owns the [profile.*] sections the inner
    // runtime build inherits (panic strategy, LTO); a profile edit
    // must regenerate the archive.
    println!("cargo:rerun-if-changed=../../Cargo.toml");
    println!("cargo:rerun-if-changed=../gossamer-codegen-cranelift/src/native.rs");
    println!("cargo:rerun-if-changed=../gossamer-codegen-cranelift/src/jit.rs");
    println!("cargo:rerun-if-changed=../gossamer-codegen-llvm/src/emit.rs");
    println!("cargo:rerun-if-changed=../gossamer-codegen-llvm/src/lower");
    println!("cargo:rerun-if-changed=../gossamer-abi/src/registry.rs");
    println!("cargo:rerun-if-env-changed=GOS_RUNTIME_LIB");
    println!("cargo:rerun-if-env-changed=GOSSAMER_SKIP_DISPATCH_PARITY");
    println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf();
    emit_rerun_if_changed_tree(&workspace_root.join("crates/gossamer-runtime/src"));

    if env::var_os("GOSSAMER_SKIP_DISPATCH_PARITY").is_none() {
        check_dispatch_parity(&workspace_root);
    }

    // Honour CARGO_TARGET_DIR if set, otherwise default to
    // <workspace>/target - the same logic cargo uses internally.
    let target_dir = env::var_os("CARGO_TARGET_DIR")
        .map_or_else(|| workspace_root.join("target"), PathBuf::from);

    let profile = env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    let lib_dir = target_dir.join(&profile);
    let lib_name = if cfg!(target_env = "msvc") {
        "gossamer_runtime.lib"
    } else {
        "libgossamer_runtime.a"
    };
    let lib_path = lib_dir.join(lib_name);

    // Force-build the runtime crate so the staticlib gets emitted.
    // Use a separate target dir to avoid a deadlock against the
    // outer cargo invocation that owns `target/`'s build lock.
    // The `rerun-if-changed` directives above only fire build.rs
    // when the runtime sources change; we always re-invoke the
    // inner cargo so it picks up source edits and refreshes the
    // staticlib in-place. Cargo's own incremental layer keeps the
    // re-run cheap when nothing has changed.
    build_runtime_into(&workspace_root, &target_dir, &profile, None);

    println!(
        "cargo:rustc-env=GOSSAMER_RUNTIME_LIB_PATH={}",
        lib_path.display()
    );

    // Linux + release: also build the runtime against musl when the
    // rustup target is installed. The `gos build --release` link
    // path consumes this archive to produce a fully static binary.
    // Skip silently when the target isn't available - we still ship
    // the dynamic path as a fallback.
    // A `-Z` GOSSAMERFLAGS toolchain (nightly sanitizers) has no musl
    // sanitizer runtime to link against, and a sanitizer-instrumented
    // gos is a debugging tool that never produces static-musl release
    // binaries - skip the musl runtime for those builds.
    let sanitizer_toolchain = env::var("GOSSAMERFLAGS").is_ok_and(|f| f.contains("-Z"));
    // Built for every profile, not just release: `gos build --release` links
    // this archive whatever profile `gos` itself was built in, so tying it to
    // the outer profile leaves a debug `gos` emitting native binaries against
    // whichever runtime a previous release build happened to leave behind.
    // The archive itself is always the release build, which is what the link
    // consumes.
    if cfg!(target_os = "linux") && !sanitizer_toolchain {
        let musl_triple = "x86_64-unknown-linux-musl";
        if rustup_target_installed(musl_triple) {
            let musl_lib_path =
                build_runtime_into(&workspace_root, &target_dir, "release", Some(musl_triple));
            publish_archive(&musl_lib_path, &lib_dir.join("libgossamer_runtime-musl.a"));
            println!(
                "cargo:rustc-env=GOSSAMER_RUNTIME_LIB_PATH_MUSL={}",
                musl_lib_path.display()
            );
        }
    }
}

fn emit_rerun_if_changed_tree(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            emit_rerun_if_changed_tree(&path);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

/// Returns true when the rustup `<triple>` target's std library is
/// installed locally - checked by probing for the rustlib dir, not
/// by shelling out to `rustup`.
fn rustup_target_installed(triple: &str) -> bool {
    let Ok(out) = Command::new(env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string()))
        .args(["--print", "sysroot"])
        .output()
    else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let sysroot = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let probe = PathBuf::from(&sysroot)
        .join("lib")
        .join("rustlib")
        .join(triple)
        .join("lib");
    probe.exists()
}

/// Invokes `cargo build -p gossamer-runtime` with an isolated target
/// directory, then copies the resulting staticlib into the outer
/// `target/<profile>/` so downstream lookups find it. When `triple`
/// is supplied, builds against that rustup target and the artifact
/// is copied into `target/<triple>/<profile>/`. Returns the path to
/// the resulting staticlib.
fn build_runtime_into(
    workspace_root: &Path,
    target_dir: &Path,
    profile: &str,
    triple: Option<&str>,
) -> PathBuf {
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    // `-Z` flags in `GOSSAMERFLAGS` are target-scoped (nightly
    // sanitizers): without an explicit `--target` the inner cargo would
    // also apply them to host proc-macros, which cannot build as
    // sanitized dylibs. Pin the inner build to the outer TARGET so the
    // flags stay on target code; the triple-aware artifact paths below
    // handle the extra directory level.
    let host_pin;
    let triple = if triple.is_none() && env::var("GOSSAMERFLAGS").is_ok_and(|f| f.contains("-Z")) {
        host_pin = env::var("TARGET").expect("cargo sets TARGET for build scripts");
        Some(host_pin.as_str())
    } else {
        triple
    };
    let inner_target = match triple {
        Some(t) => target_dir.join(format!("runtime-staticlib-{t}")),
        None => target_dir.join("runtime-staticlib"),
    };

    let mut cmd = Command::new(&cargo);
    cmd.arg("build")
        .arg("-p")
        .arg("gossamer-runtime")
        .arg("--target-dir")
        .arg(&inner_target)
        .current_dir(workspace_root);
    if macos_deployment::is_macos_target(triple, cfg!(target_os = "macos")) {
        let deployment_target = macos_deployment::effective_deployment_target();
        macos_deployment::set_command_deployment_target(&mut cmd, &deployment_target);
    }
    // Keep the static archive used by `gos build` in step with the runtime
    // linked into the CLI itself.
    if env::var_os("CARGO_FEATURE_INLINE_VEC_WORDS_6").is_some() {
        cmd.arg("--features").arg("inline-vec-words-6");
    }
    if env::var_os("CARGO_FEATURE_INLINE_VEC_WORDS_8").is_some() {
        cmd.arg("--features").arg("inline-vec-words-8");
    }
    if let Some(t) = triple {
        cmd.arg("--target").arg(t);
    }
    if profile == "release" {
        cmd.arg("--release");
    }
    // Strip cargo-set vars that bias the inner build toward this
    // crate's flags. The outer-cargo `RUSTFLAGS` is removed so
    // workspace-wide flags (CI's `-D warnings`, IDE toggles, etc.)
    // can't leak into the runtime build. The Gossamer-internal
    // codegen knobs are owned by this build script (release adds
    // `function-sections`/`data-sections`); the only user-facing
    // override is `GOSSAMERFLAGS`, which the cli forwards as
    // `RUSTFLAGS` to the inner cargo invocation. We deliberately
    // do NOT honour the outer `RUSTFLAGS` even as a fallback.
    // `CARGO_ENCODED_RUSTFLAGS` is the form cargo actually sets and it wins
    // over `RUSTFLAGS`, so both have to go for the inner build to be
    // isolated.
    for var in [
        "CARGO_PRIMARY_PACKAGE",
        "CARGO_PKG_NAME",
        "RUSTC_WRAPPER",
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
    ] {
        cmd.env_remove(var);
    }
    // Per-function ELF sections (so the user binary's `--gc-sections` can
    // prune unused `gos_rt_*` helpers) are what rustc already emits by
    // default on the targets this builds for; there is no stable `-C`
    // spelling to request them, so the inner build takes only the
    // user-facing `GOSSAMERFLAGS` override.
    let mut flags: Vec<String> = Vec::new();
    // The heap sampler runs inside the global allocator and reaches the
    // Gossamer function that allocated by following the frame-pointer chain
    // up through these shims. Compiled bodies already carry
    // `"frame-pointer"="all"`; without the same guarantee here the chain
    // breaks at the first runtime frame and every heap profile comes back
    // empty. Scoped to this archive, so the interpreter and the JIT - where
    // forcing frame pointers costs double digits on tight loops - keep the
    // register.
    flags.push("-C force-frame-pointers=yes".to_string());
    if let Ok(extra) = env::var("GOSSAMERFLAGS") {
        if !extra.trim().is_empty() {
            flags.push(extra);
        }
    }
    if !flags.is_empty() {
        cmd.env("RUSTFLAGS", flags.join(" "));
    }

    let status = cmd.status().expect("invoke cargo for runtime build");
    assert!(
        status.success(),
        "failed to build gossamer-runtime staticlib (triple={triple:?})"
    );

    let lib_name = if cfg!(target_env = "msvc") {
        "gossamer_runtime.lib"
    } else {
        "libgossamer_runtime.a"
    };
    let inner_profile_dir = match triple {
        Some(t) => inner_target.join(t).join(profile),
        None => inner_target.join(profile),
    };
    let inner_artifact = inner_profile_dir.join(lib_name);
    let outer_profile_dir = match triple {
        Some(t) => target_dir.join(t).join(profile),
        None => target_dir.join(profile),
    };
    let outer_artifact = outer_profile_dir.join(lib_name);
    if let Some(parent) = outer_artifact.parent() {
        std::fs::create_dir_all(parent).expect("create outer profile dir");
    }
    // Publish the (large, ~300 MB) staticlib atomically: copy to a
    // unique temp path in the same directory, then rename into place.
    // A plain `fs::copy` truncates the destination and streams the bytes,
    // so anything that reads `libgossamer_runtime.a` while this build
    // script re-runs (it re-runs whenever a `GOS_*` env var changes - the
    // diagnose CI step sets several) sees a partially written file and the
    // linker fails with "file truncated". `rename` is atomic on the same
    // filesystem, so a reader always sees either the old or the new
    // complete archive, never a half-written one.
    publish_archive(&inner_artifact, &outer_artifact);
    outer_artifact
}

/// Copies a runtime archive atomically so concurrent package/build readers
/// never observe a partially-written static library.
fn publish_archive(source: &Path, destination: &Path) {
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).expect("create runtime archive directory");
    }
    let tmp = destination.with_extension(format!("a.tmp-{}", std::process::id()));
    std::fs::copy(source, &tmp).unwrap_or_else(|e| {
        panic!(
            "copy staticlib {} -> {}: {e}",
            source.display(),
            tmp.display()
        )
    });
    std::fs::rename(&tmp, destination).expect("atomically publish staticlib");
}

/// Fails the build if any `gos_rt_*` symbol declared in `c_abi.rs`
/// is not referenced by any codegen and is not on the allowlist.
#[allow(
    clippy::too_many_lines,
    reason = "directory-walking scaffold is linear; splitting would obscure the read"
)]
fn check_dispatch_parity(workspace_root: &Path) {
    // c_abi.rs was split into a directory of domain submodules
    // (crates/gossamer-runtime/src/c_abi/{vec,map,str,...}.rs) plus
    // a slim hub at crates/gossamer-runtime/src/c_abi.rs that
    // declares the submodules. Concatenate every .rs under the
    // c_abi/ directory so the symbol scan covers all submodules.
    let mut c_abi = read_text(workspace_root.join("crates/gossamer-runtime/src/c_abi.rs"));
    let c_abi_dir = workspace_root.join("crates/gossamer-runtime/src/c_abi");
    if c_abi_dir.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(&c_abi_dir)
            .unwrap_or_else(|e| panic!("dispatch-parity: read_dir {}: {e}", c_abi_dir.display()))
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("rs"))
            .collect();
        entries.sort();
        for path in entries {
            c_abi.push_str(&read_text(path));
            c_abi.push('\n');
        }
    }
    // native.rs was split into a `native/` directory of submodules.
    // Concatenate every .rs under it.
    let mut cl_native = String::new();
    let cl_native_dir = workspace_root.join("crates/gossamer-codegen-cranelift/src/native");
    if cl_native_dir.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(&cl_native_dir)
            .unwrap_or_else(|e| {
                panic!("dispatch-parity: read_dir {}: {e}", cl_native_dir.display())
            })
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("rs"))
            .collect();
        entries.sort();
        for path in entries {
            cl_native.push_str(&read_text(path));
            cl_native.push('\n');
        }
    } else {
        cl_native =
            read_text(workspace_root.join("crates/gossamer-codegen-cranelift/src/native.rs"));
    }
    let cl_jit = read_text(workspace_root.join("crates/gossamer-codegen-cranelift/src/jit.rs"));
    let llvm_emit = read_text(workspace_root.join("crates/gossamer-codegen-llvm/src/emit.rs"));
    // codegen-llvm/src/lower.rs was split into lower/ directory
    let mut llvm_lower = String::new();
    let llvm_lower_dir = workspace_root.join("crates/gossamer-codegen-llvm/src/lower");
    if llvm_lower_dir.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(&llvm_lower_dir)
            .unwrap_or_else(|e| {
                panic!(
                    "dispatch-parity: read_dir {}: {e}",
                    llvm_lower_dir.display()
                )
            })
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("rs"))
            .collect();
        entries.sort();
        for path in entries {
            llvm_lower.push_str(&read_text(path));
            llvm_lower.push('\n');
        }
    } else {
        llvm_lower = read_text(workspace_root.join("crates/gossamer-codegen-llvm/src/lower.rs"));
    }
    // The typed ABI registry is the single source of truth for all gos_rt_*
    // symbols used by both LLVM and Cranelift backends. It replaces the
    // old RUNTIME_DECLARATIONS string array, so the scanner must include it.
    let abi_registry = read_text(workspace_root.join("crates/gossamer-abi/src/registry.rs"));

    let defined = extract_runtime_definitions(&c_abi);
    let mut referenced: BTreeSet<String> = BTreeSet::new();
    referenced.extend(extract_referenced_symbols(&cl_native));
    referenced.extend(extract_referenced_symbols(&cl_jit));
    referenced.extend(extract_referenced_symbols(&llvm_emit));
    referenced.extend(extract_referenced_symbols(&llvm_lower));
    referenced.extend(extract_referenced_symbols(&abi_registry));

    let allowed: BTreeSet<String> = KNOWN_UNUSED_RUNTIME_SYMBOLS
        .iter()
        .map(|s| (*s).to_string())
        .collect();

    let mut orphans: Vec<String> = defined
        .iter()
        .filter(|sym| !referenced.contains(sym.as_str()) && !allowed.contains(sym.as_str()))
        .cloned()
        .collect();
    orphans.sort();

    let mut stale_allowlist: Vec<String> = allowed
        .iter()
        .filter(|sym| !defined.contains(sym.as_str()))
        .cloned()
        .collect();
    stale_allowlist.sort();

    if !orphans.is_empty() {
        let lines = orphans.join("\n  ");
        panic!(
            "dispatch-table parity check failed.\n\
             {n} runtime symbol(s) declared in crates/gossamer-runtime/src/c_abi.rs \
             have no corresponding reference in any codegen file:\n  {lines}\n\
             Wire each one through the appropriate codegen, or add it to \
             KNOWN_UNUSED_RUNTIME_SYMBOLS in crates/gossamer-cli/build.rs with a \
             one-line comment justifying the omission. Set \
             GOSSAMER_SKIP_DISPATCH_PARITY=1 to bypass during local debugging.",
            n = orphans.len(),
        );
    }
    if !stale_allowlist.is_empty() {
        let lines = stale_allowlist.join("\n  ");
        panic!(
            "dispatch-table parity check failed.\n\
             {n} symbol(s) listed in KNOWN_UNUSED_RUNTIME_SYMBOLS no longer exist \
             in crates/gossamer-runtime/src/c_abi.rs:\n  {lines}\n\
             Remove the stale entries from build.rs.",
            n = stale_allowlist.len(),
        );
    }
}

fn read_text(path: PathBuf) -> String {
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("dispatch-parity: read {}: {err}", path.display()))
}

/// Returns every `gos_rt_*` symbol whose Rust definition appears in
/// `c_abi.rs`. The author convention is `pub unsafe extern "C" fn
/// gos_rt_<name>` or `pub extern "C" fn gos_rt_<name>`; both are
/// matched by anchoring on the `extern "C"` clause.
fn extract_runtime_definitions(src: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in src.lines() {
        let Some(rest) = line.split_once("extern \"C\" fn ").map(|(_, r)| r) else {
            continue;
        };
        let name: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if name.starts_with("gos_rt_") {
            out.insert(name);
        }
    }
    out
}

/// Returns every `gos_rt_*` identifier mentioned anywhere in `src`.
/// We accept any occurrence - string literal in a match arm, LLVM
/// IR `declare`, JIT mapping, or even a comment - because the
/// parity check is an "is this symbol live somewhere" probe, not a
/// per-codegen wiring audit. The unique false-negative this allows
/// (a stale comment "documenting" a symbol that has no real call
/// site) is not worth the parser complexity to filter out.
fn extract_referenced_symbols(src: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let bytes = src.as_bytes();
    let needle = b"gos_rt_";
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
            // Reject if preceded by an identifier char - we only
            // want symbol-name occurrences, not "_gos_rt_…" or
            // mid-identifier substring matches.
            let prev_is_ident = i > 0 && is_ident_byte(bytes[i - 1]);
            if !prev_is_ident {
                let mut j = i + needle.len();
                while j < bytes.len() && is_ident_byte(bytes[j]) {
                    j += 1;
                }
                if j > i + needle.len()
                    && let Ok(s) = std::str::from_utf8(&bytes[i..j])
                {
                    out.insert(s.to_string());
                }
                i = j;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}
