//! `gos build` for a `[lib]` whose manifest names a `kind`: a static archive
//! and a shared library holding the library's `#[export]` functions with the
//! runtime they need, and `include/<name>.h` declaring them.
//!
//! The archive carries the runtime's objects, so a C build links it alone
//! (plus the system libraries the build reports). The shared library exports
//! only the `#[export]` symbols: a version script on Linux, an exported
//! symbols list on macOS, and a module-definition file on Windows keep the
//! runtime's own symbols inside it, so two Gossamer libraries load side by
//! side.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Result, anyhow};
use gossamer_pkg::LibKind;

use super::c_header::{self, Export};
use super::{
    BuildRequest, BuildTimings, LinkTarget, NativeBuildError, NativeLinks, TargetEnv, TargetOs,
    ValidatedSource, build_static_bindings_lib, emit_native_objects, ensure_output_dir,
    find_runtime_lib, find_runtime_lib_for_target, host_os, resolve_link_target,
    trace_link_command, validate_source,
};
use crate::paths::read_entry_unit;

/// A library build a manifest asks for.
#[derive(Debug, Clone)]
pub(super) struct LibraryTarget {
    /// The artifact name: `lib.name`, or the project id's last segment,
    /// with `-` written `_`.
    name: String,
    kinds: Vec<LibKind>,
}

/// The library build `file` is the entry of, when its manifest's `[lib]`
/// names a `kind` and `file` is that library's root.
pub(super) fn target_for(file: &Path) -> Result<Option<LibraryTarget>> {
    let Some(manifest_path) = gossamer_pkg::find_manifest(file) else {
        return Ok(None);
    };
    let text = fs::read_to_string(&manifest_path)
        .map_err(|e| anyhow!("reading {}: {e}", manifest_path.display()))?;
    let manifest = gossamer_pkg::Manifest::parse(&text)
        .map_err(|e| anyhow!("parsing {}: {e}", manifest_path.display()))?;
    let Some(lib) = manifest.lib.as_ref().filter(|lib| !lib.kind.is_empty()) else {
        return Ok(None);
    };
    let Some(root) = manifest_path.parent() else {
        return Ok(None);
    };
    let candidates = match &lib.path {
        Some(path) => vec![root.join(path)],
        None => vec![root.join("src").join("lib.gos"), root.join("lib.gos")],
    };
    let Some(entry) = candidates.into_iter().find(|path| path.is_file()) else {
        return Ok(None);
    };
    let same = |a: &Path, b: &Path| match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if !same(&entry, file) {
        return Ok(None);
    }
    let name = lib.name.clone().unwrap_or_else(|| {
        let id = manifest.project.id.to_string();
        id.rsplit('/').next().unwrap_or(&id).to_string()
    });
    let name = name.replace('-', "_");
    Ok(Some(LibraryTarget {
        name,
        kinds: lib.kind.clone(),
    }))
}

/// Builds the artifacts `target` names from the library rooted at `file`.
pub(super) fn build(file: &Path, request: &BuildRequest<'_>, target: &LibraryTarget) -> Result<()> {
    let opts = request.link;
    let lt = resolve_link_target(request.target);
    check_library_target(&lt)?;
    let mut timings = BuildTimings::default();
    let unit = read_entry_unit(file)?;
    let ValidatedSource {
        sf,
        resolutions,
        table,
        tcx,
        program_end,
        ..
    } = validate_source(
        file,
        unit.source,
        &mut timings,
        (unit.entry.as_path(), unit.origins.as_slice()),
    )?;
    let exports = library_exports(&sf, file, &target.name)?;
    let header = c_header::render(&sf, &exports, &target.name);
    let links = gossamer_driver::foreign_link_libraries(&sf, program_end);
    let libraries = NativeLinks {
        search: crate::paths::foreign_search_dirs(file, &links),
        libraries: links.libraries,
    };
    let out_dir = output_dir(file, opts.release)?;
    let tmp_dir = std::env::temp_dir().join(format!(
        "gos-build-{}-lib{}",
        std::process::id(),
        target.name
    ));
    fs::create_dir_all(&tmp_dir).map_err(|err| anyhow!("creating {}: {err}", tmp_dir.display()))?;
    let checked = gossamer_driver::CheckedFrontend {
        sf,
        resolutions,
        table,
        tcx,
    };
    gossamer_codegen_llvm::set_library_name(target.name.clone());
    let started = Instant::now();
    let result = (|| -> std::result::Result<Vec<PathBuf>, NativeBuildError> {
        let (objects, _) = emit_native_objects(
            &target.name,
            &tmp_dir,
            opts.release,
            false,
            checked,
            &mut timings,
        )?;
        let runtime = if lt.is_cross {
            find_runtime_lib_for_target(&lt.triple)?
        } else {
            find_runtime_lib()?
        };
        let native =
            crate::native_sources::archives(file, lt.is_cross.then_some(lt.triple.as_str()))
                .map_err(|err| NativeBuildError::BackendFailed(format!("{err:#}")))?;
        let native: Vec<PathBuf> = native
            .archives
            .into_iter()
            .map(|archive| archive.path)
            .collect();
        let bindings_target = lt.is_cross.then_some(lt.triple.as_str());
        let bindings = build_static_bindings_lib(file, opts.release, bindings_target)
            .map_err(|err| NativeBuildError::LinkerMissing(format!("rust-bindings: {err}")))?;
        let parts = Parts {
            lt: &lt,
            objects: &objects,
            runtime: &runtime,
            bindings: bindings.as_deref(),
            native: &native,
            libraries: &libraries,
            exports: &exports,
            tmp_dir: &tmp_dir,
            strip: !opts.debug_info,
        };
        let mut written = Vec::new();
        for kind in &target.kinds {
            let path = out_dir.join(artifact_file_name(&target.name, *kind, lt.os));
            ensure_output_dir(&path)?;
            match kind {
                LibKind::Staticlib => archive(&parts, &path)?,
                LibKind::Cdylib => shared(&parts, &target.name, &path)?,
            }
            written.push(path);
        }
        Ok(written)
    })();
    let keep_artifacts = std::env::var_os("GOS_LLVM_DUMP").is_some()
        || std::env::var_os("GOS_KEEP_BUILD_ARTIFACTS").is_some();
    if !keep_artifacts {
        let _ = fs::remove_dir_all(&tmp_dir);
    }
    let written = result.map_err(|err| anyhow!("build: {}", err.user_message()))?;
    report(&out_dir, target, &header, &written, &lt, &libraries)?;
    if request.timings {
        timings.total = started.elapsed();
        timings.print(false);
    }
    Ok(())
}

/// Refuses a target a library cannot be built for from this host.
fn check_library_target(lt: &LinkTarget) -> Result<()> {
    if lt.is_cross && host_os() != lt.os {
        return Err(anyhow!(
            "build: a library is built for the host's operating system; `{}` is another",
            lt.triple
        ));
    }
    if lt.env == TargetEnv::Musl {
        return Err(anyhow!(
            "build: a library links against its host's C library; `{}` is a static musl target",
            lt.triple
        ));
    }
    Ok(())
}

/// The `#[export]` functions of the library `name`, which must have some
/// and must leave `main` to its host.
fn library_exports(sf: &gossamer_ast::SourceFile, file: &Path, name: &str) -> Result<Vec<Export>> {
    if c_header::declares_main(sf) {
        return Err(anyhow!(
            "build: the library root {} declares `fn main`; a library's host owns `main`, \
             so its entry points are `#[export]` functions",
            file.display()
        ));
    }
    let exports = c_header::exports(sf);
    if exports.is_empty() {
        return Err(anyhow!(
            "build: the library {name} names `[lib] kind` but exports nothing; mark the \
             functions C calls with `#[export]`"
        ));
    }
    Ok(exports)
}

/// Writes the header beside the artifacts and says what was built and how
/// a host links it.
fn report(
    out_dir: &Path,
    target: &LibraryTarget,
    header: &str,
    written: &[PathBuf],
    lt: &LinkTarget,
    libraries: &NativeLinks,
) -> Result<()> {
    let include = out_dir.join("include");
    fs::create_dir_all(&include).map_err(|e| anyhow!("creating {}: {e}", include.display()))?;
    let header_path = include.join(format!("{}.h", target.name));
    fs::write(&header_path, header)
        .map_err(|e| anyhow!("writing {}: {e}", header_path.display()))?;
    for path in written {
        let size = fs::metadata(path).map_or(0, |m| m.len());
        println!("build: {size}B library at {}", path.display());
    }
    println!("build: C header at {}", header_path.display());
    if target.kinds.contains(&LibKind::Staticlib) {
        println!(
            "build: link the archive with: {}",
            static_link_libraries(lt, libraries).join(" ")
        );
    }
    Ok(())
}

/// `target/<profile>/` beside the manifest, or beside `file` without one.
fn output_dir(file: &Path, release: bool) -> Result<PathBuf> {
    let profile = if release { "release" } else { "debug" };
    let base = gossamer_pkg::find_manifest(file)
        .and_then(|manifest| manifest.parent().map(Path::to_path_buf))
        .or_else(|| file.parent().map(Path::to_path_buf))
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("target").join(profile);
    fs::create_dir_all(&dir).map_err(|e| anyhow!("creating {}: {e}", dir.display()))?;
    Ok(dir)
}

/// The file an artifact of `kind` is written to on `os`.
fn artifact_file_name(name: &str, kind: LibKind, os: TargetOs) -> String {
    match (kind, os) {
        (LibKind::Staticlib, TargetOs::Windows) if cfg!(target_env = "msvc") => {
            format!("{name}.lib")
        }
        (LibKind::Staticlib, _) => format!("lib{name}.a"),
        (LibKind::Cdylib, TargetOs::Windows) => format!("{name}.dll"),
        (LibKind::Cdylib, TargetOs::MacOs) => format!("lib{name}.dylib"),
        (LibKind::Cdylib, _) => format!("lib{name}.so"),
    }
}

/// The system libraries a C program linking the static archive names after
/// it: the runtime's, then the library's own foreign ones.
fn static_link_libraries(lt: &LinkTarget, libraries: &NativeLinks) -> Vec<String> {
    let msvc = lt.os == TargetOs::Windows && cfg!(target_env = "msvc");
    let mut flags: Vec<String> = Vec::new();
    for dir in &libraries.search {
        flags.push(if msvc {
            format!("/LIBPATH:{}", dir.display())
        } else {
            format!("-L{}", dir.display())
        });
    }
    for library in &libraries.libraries {
        flags.push(if msvc {
            format!("{library}.lib")
        } else {
            format!("-l{library}")
        });
    }
    let system: &[&str] = match lt.os {
        TargetOs::Linux => &["-lpthread", "-ldl", "-lm"],
        TargetOs::Windows if cfg!(target_env = "msvc") => &[
            "advapi32.lib",
            "bcrypt.lib",
            "kernel32.lib",
            "ntdll.lib",
            "userenv.lib",
            "ws2_32.lib",
            "synchronization.lib",
            "dbghelp.lib",
        ],
        TargetOs::Windows => &["-lws2_32", "-lbcrypt", "-ladvapi32", "-luserenv", "-lntdll"],
        TargetOs::MacOs | TargetOs::Other => &["-lpthread", "-lm"],
    };
    flags.extend(system.iter().map(ToString::to_string));
    flags
}

/// What every artifact is linked from.
struct Parts<'a> {
    lt: &'a LinkTarget,
    objects: &'a [PathBuf],
    runtime: &'a Path,
    /// The `[rust-bindings]` archive, which carries its own copy of the
    /// runtime.
    bindings: Option<&'a Path>,
    /// The archives of the `[native]` sources.
    native: &'a [PathBuf],
    libraries: &'a NativeLinks,
    exports: &'a [Export],
    tmp_dir: &'a Path,
    strip: bool,
}

impl Parts<'_> {
    /// The archives whose members the artifact takes: the `[native]`
    /// sources' first, which call into what follows, then the runtime. The
    /// bindings archive stands in for the runtime when there is one, since
    /// it holds the same runtime objects and an archive may define each
    /// symbol once.
    fn archives(&self) -> Vec<&Path> {
        let mut archives: Vec<&Path> = self.native.iter().map(PathBuf::as_path).collect();
        archives.push(self.bindings.unwrap_or(self.runtime));
        archives
    }
}

fn run_tool(mut cmd: std::process::Command) -> std::result::Result<(), NativeBuildError> {
    trace_link_command(&cmd);
    let program = cmd.get_program().to_string_lossy().into_owned();
    match cmd.status() {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(NativeBuildError::LinkerFailed(format!(
            "{program} exited with {status}"
        ))),
        Err(err) => Err(NativeBuildError::LinkerMissing(format!("{program}: {err}"))),
    }
}

/// Writes the static archive: the program's objects and every member of
/// the runtime archive.
fn archive(parts: &Parts<'_>, out: &Path) -> std::result::Result<(), NativeBuildError> {
    let _ = fs::remove_file(out);
    match parts.lt.os {
        TargetOs::MacOs => {
            let mut cmd = std::process::Command::new("libtool");
            cmd.arg("-static").arg("-o").arg(out);
            cmd.args(parts.objects);
            cmd.args(parts.archives());
            run_tool(cmd)
        }
        TargetOs::Windows if cfg!(target_env = "msvc") => {
            let mut cmd = std::process::Command::new(rust_lld()?);
            // `/lib` as the first argument makes lld-link act as lib.exe,
            // which merges the members of the archives it is given.
            cmd.arg("-flavor").arg("link").arg("/lib").arg("/NOLOGO");
            let mut out_arg = std::ffi::OsString::from("/OUT:");
            out_arg.push(out);
            cmd.arg(out_arg);
            cmd.args(parts.objects);
            cmd.args(parts.archives());
            run_tool(cmd)
        }
        _ => mri_archive(parts, out),
    }
}

/// Merges archives with an `ar` MRI script, which keeps members of the same
/// name apart where extracting them to one directory would not. The script
/// names files the build copied into its own directory, whose paths hold no
/// character the script language treats specially.
fn mri_archive(parts: &Parts<'_>, out: &Path) -> std::result::Result<(), NativeBuildError> {
    let io = |what: &str, path: &Path, err: std::io::Error| {
        NativeBuildError::Io(anyhow!("{what} {}: {err}", path.display()))
    };
    let staged = parts.tmp_dir.join("archive");
    fs::create_dir_all(&staged).map_err(|e| io("creating", &staged, e))?;
    let mut script = String::new();
    let built = staged.join("out.a");
    script.push_str(&format!("CREATE {}\n", built.display()));
    for (index, object) in parts.objects.iter().enumerate() {
        let copy = staged.join(format!("gos{index}.o"));
        fs::copy(object, &copy).map_err(|e| io("copying", object, e))?;
        script.push_str(&format!("ADDMOD {}\n", copy.display()));
    }
    for (index, archive) in parts.archives().into_iter().enumerate() {
        let copy = staged.join(format!("runtime{index}.a"));
        fs::copy(archive, &copy).map_err(|e| io("copying", archive, e))?;
        script.push_str(&format!("ADDLIB {}\n", copy.display()));
    }
    script.push_str("SAVE\nEND\n");
    let script_path = staged.join("archive.mri");
    fs::write(&script_path, &script).map_err(|e| io("writing", &script_path, e))?;
    let ar = std::env::var("AR").unwrap_or_else(|_| "ar".to_string());
    let mut cmd = std::process::Command::new(&ar);
    cmd.arg("-M");
    let script_file = fs::File::open(&script_path).map_err(|e| io("opening", &script_path, e))?;
    cmd.stdin(script_file);
    run_tool(cmd)?;
    fs::copy(&built, out).map_err(|e| io("writing", out, e))?;
    Ok(())
}

/// Links the shared library exporting only the `#[export]` symbols.
fn shared(parts: &Parts<'_>, name: &str, out: &Path) -> std::result::Result<(), NativeBuildError> {
    let io = |path: &Path, err: std::io::Error| {
        NativeBuildError::Io(anyhow!("writing {}: {err}", path.display()))
    };
    let shutdown = format!("{name}_shutdown");
    let symbols: Vec<&str> = parts
        .exports
        .iter()
        .map(|e| e.symbol.as_str())
        .chain(std::iter::once(shutdown.as_str()))
        .collect();
    if parts.lt.os == TargetOs::Windows && cfg!(target_env = "msvc") {
        return shared_msvc(parts, name, out, &symbols);
    }
    let cc = if parts.lt.is_cross {
        super::cross_cc(parts.lt)
    } else {
        std::env::var("CC").unwrap_or_else(|_| "cc".to_string())
    };
    let mut cmd = std::process::Command::new(&cc);
    match parts.lt.os {
        TargetOs::MacOs => {
            super::configure_macos_link_command(&mut cmd);
            let list = parts.tmp_dir.join("exports.list");
            let mut text = String::new();
            for symbol in &symbols {
                text.push('_');
                text.push_str(symbol);
                text.push('\n');
            }
            fs::write(&list, text).map_err(|e| io(&list, e))?;
            cmd.arg("-dynamiclib");
            cmd.arg(format!("-Wl,-install_name,@rpath/lib{name}.dylib"));
            cmd.arg(format!("-Wl,-exported_symbols_list,{}", list.display()));
            cmd.arg("-Wl,-dead_strip");
        }
        TargetOs::Windows => {
            let def = parts.tmp_dir.join(format!("{name}.def"));
            fs::write(&def, module_definition(name, &symbols)).map_err(|e| io(&def, e))?;
            cmd.arg("-shared");
            cmd.arg(&def);
            let implib = out.with_file_name(format!("lib{name}.dll.a"));
            cmd.arg(format!("-Wl,--out-implib,{}", implib.display()));
        }
        TargetOs::Linux | TargetOs::Other => {
            let script = parts.tmp_dir.join("exports.map");
            let mut globals = String::new();
            for symbol in &symbols {
                globals.push_str("    ");
                globals.push_str(symbol);
                globals.push_str(";\n");
            }
            fs::write(
                &script,
                format!("{{\n  global:\n{globals}  local: *;\n}};\n"),
            )
            .map_err(|e| io(&script, e))?;
            cmd.arg("-shared");
            cmd.arg(format!("-Wl,-soname,lib{name}.so"));
            cmd.arg(format!("-Wl,--version-script={}", script.display()));
            cmd.arg("-Wl,--gc-sections");
            if parts.strip {
                cmd.arg("-Wl,--strip-debug");
            }
        }
    }
    cmd.args(parts.objects);
    cmd.args(parts.archives());
    for dir in &parts.libraries.search {
        cmd.arg(format!("-L{}", dir.display()));
    }
    for library in &parts.libraries.libraries {
        cmd.arg(format!("-l{library}"));
    }
    cmd.arg("-o").arg(out);
    cmd.arg("-lpthread");
    if parts.lt.os == TargetOs::Linux {
        cmd.arg("-ldl");
    }
    cmd.arg("-lm");
    if parts.lt.os == TargetOs::Windows {
        for lib in ["ws2_32", "bcrypt", "advapi32", "userenv", "ntdll"] {
            cmd.arg(format!("-l{lib}"));
        }
    }
    run_tool(cmd)
}

#[cfg(windows)]
fn rust_lld() -> std::result::Result<PathBuf, NativeBuildError> {
    super::locate_rust_lld()
}

#[cfg(not(windows))]
fn rust_lld() -> std::result::Result<PathBuf, NativeBuildError> {
    Err(NativeBuildError::LinkerMissing(
        "the Windows MSVC linker runs on a Windows host".to_string(),
    ))
}

/// A Windows module-definition file exporting `symbols` from `name.dll`.
fn module_definition(name: &str, symbols: &[&str]) -> String {
    let mut text = format!("LIBRARY {name}\nEXPORTS\n");
    for symbol in symbols {
        text.push_str(&format!("    {symbol}\n"));
    }
    text
}

/// The MSVC shared library: `name.dll` and its import library
/// `name.dll.lib`, beside a static `name.lib` without colliding with it.
fn shared_msvc(
    parts: &Parts<'_>,
    name: &str,
    out: &Path,
    symbols: &[&str],
) -> std::result::Result<(), NativeBuildError> {
    let def = parts.tmp_dir.join(format!("{name}.def"));
    fs::write(&def, module_definition(name, symbols))
        .map_err(|e| NativeBuildError::Io(anyhow!("writing {}: {e}", def.display())))?;
    let mut cmd = std::process::Command::new(rust_lld()?);
    cmd.arg("-flavor").arg("link").arg("/NOLOGO").arg("/DLL");
    let mut out_arg = std::ffi::OsString::from("/OUT:");
    out_arg.push(out);
    cmd.arg(out_arg);
    let mut def_arg = std::ffi::OsString::from("/DEF:");
    def_arg.push(&def);
    cmd.arg(def_arg);
    let mut implib = std::ffi::OsString::from("/IMPLIB:");
    implib.push(out.with_file_name(format!("{name}.dll.lib")));
    cmd.arg(implib);
    cmd.args(parts.objects);
    cmd.args(parts.archives());
    for dir in &parts.libraries.search {
        cmd.arg(format!("/LIBPATH:{}", dir.display()));
    }
    for library in &parts.libraries.libraries {
        cmd.arg(format!("{library}.lib"));
    }
    for lib in super::MSVC_SYSTEM_LIBRARIES {
        cmd.arg(lib);
    }
    run_tool(cmd)
}
