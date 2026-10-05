//! `gos env` - prints the toolchain environment in `key  value`
//! format (matches `go env`'s shape). Surfaces every datapoint a
//! user typically needs when diagnosing an install / build issue.

use crate::cmd::build;
use crate::paths::find_project_root;

/// Entry point for `gos env`.
pub(crate) fn run() {
    let runtime = match build::find_runtime_lib() {
        Ok(p) => p.display().to_string(),
        Err(e) => format!("<not found: {}>", e.user_message()),
    };
    let cc = std::env::var("CC").unwrap_or_else(|_| {
        if cfg!(windows) {
            "rust-lld.exe".to_string()
        } else {
            "cc".to_string()
        }
    });
    let host = std::env::var("HOST").unwrap_or_else(|_| {
        format!(
            "{arch}-{os}",
            arch = std::env::consts::ARCH,
            os = std::env::consts::OS,
        )
    });
    let target_dir = std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".to_string());
    let project = match find_project_root() {
        Some(p) => p.join("project.toml").display().to_string(),
        None => "<no project.toml above cwd>".to_string(),
    };
    let cwd = match std::env::current_dir() {
        Ok(p) => p.display().to_string(),
        Err(_) => "<unreadable>".to_string(),
    };

    let llvm: Vec<(String, String)> = gossamer_codegen_llvm::llvm_toolchain_status()
        .into_iter()
        .map(|status| {
            (
                format!("llvm_{}", status.tool),
                llvm_tool_line(status.resolved),
            )
        })
        .collect();
    let native_cc = crate::native_sources::compiler_description();
    let mut pairs: Vec<(&str, &str)> = vec![
        ("gos_version", env!("CARGO_PKG_VERSION")),
        ("runtime_lib", &runtime),
        ("cc", &cc),
        ("native_cc", &native_cc),
    ];
    pairs.extend(llvm.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    pairs.extend([
        ("host", host.as_str()),
        ("target_dir", target_dir.as_str()),
        ("project", project.as_str()),
        ("cwd", cwd.as_str()),
    ]);
    let width = pairs.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    for (k, v) in pairs {
        outln!("{k:<width$}  {v}");
    }
}

/// One LLVM tool's line: its path and major, or why a native build cannot
/// use it. Only the first line of a failure is shown; `gos build` prints the
/// install instructions in full.
fn llvm_tool_line(resolved: Result<(std::path::PathBuf, Option<u32>), String>) -> String {
    match resolved {
        Ok((path, Some(major))) => format!("{} (LLVM {major})", path.display()),
        Ok((path, None)) => format!("{} (version unknown)", path.display()),
        Err(reason) => format!("<{}>", reason.lines().next().unwrap_or_default()),
    }
}
