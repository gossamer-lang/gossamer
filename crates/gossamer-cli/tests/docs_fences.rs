//! Every Gossamer code fence in `docs_src` is a program `gos check` accepts,
//! unless the line before it says otherwise: `<!-- fragment -->` marks a
//! piece of a larger program the prose describes, and
//! `<!-- compile_fail CODE -->` marks an example that must be rejected with
//! that diagnostic.

use std::path::{Path, PathBuf};
use std::process::Command;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the CLI crate sits two levels below the workspace root")
        .to_path_buf()
}

/// What a fence's marker asks of it.
enum Expect {
    Checks,
    Fragment,
    Rejected(String),
}

struct Fence {
    file: PathBuf,
    line: usize,
    expect: Expect,
    code: String,
}

fn markdown_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            markdown_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "md") {
            out.push(path);
        }
    }
}

fn fences_in(file: &Path) -> Vec<Fence> {
    let text = std::fs::read_to_string(file).expect("read markdown");
    let lines: Vec<&str> = text.lines().collect();
    let mut fences = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let info = lines[i].trim_start();
        if !(info == "```gossamer" || info == "```gos") {
            i += 1;
            continue;
        }
        let marker = lines[..i]
            .iter()
            .rev()
            .find(|l| !l.trim().is_empty())
            .map_or("", |l| l.trim());
        let expect = if marker == "<!-- fragment -->" {
            Expect::Fragment
        } else if let Some(code) = marker
            .strip_prefix("<!-- compile_fail ")
            .and_then(|rest| rest.strip_suffix(" -->"))
        {
            Expect::Rejected(code.trim().to_string())
        } else {
            Expect::Checks
        };
        let start = i + 1;
        let mut end = start;
        while end < lines.len() && lines[end].trim_start() != "```" {
            end += 1;
        }
        fences.push(Fence {
            file: file.to_path_buf(),
            line: i + 1,
            expect,
            code: lines[start..end].join("\n") + "\n",
        });
        i = end + 1;
    }
    fences
}

#[test]
fn every_docs_fence_checks_or_says_why_not() {
    let root = workspace_root();
    let mut files = Vec::new();
    markdown_files(&root.join("docs_src"), &mut files);
    files.sort();
    let scratch = std::env::temp_dir().join(format!("gos-doc-fences-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).expect("scratch dir");
    let mut failures = Vec::new();
    let mut checked = 0;
    for fence in files.iter().flat_map(|f| fences_in(f)) {
        if matches!(fence.expect, Expect::Fragment) {
            continue;
        }
        checked += 1;
        let program = scratch.join(format!("fence_{checked}.gos"));
        std::fs::write(&program, &fence.code).expect("write fence");
        let output = Command::new(env!("CARGO_BIN_EXE_gos"))
            .arg("check")
            .arg(&program)
            .output()
            .expect("run gos check");
        let report = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let place = format!(
            "{}:{}",
            fence
                .file
                .strip_prefix(&root)
                .unwrap_or(&fence.file)
                .display(),
            fence.line
        );
        match &fence.expect {
            Expect::Checks if !output.status.success() => {
                let first = report
                    .lines()
                    .find(|l| l.starts_with("error"))
                    .unwrap_or("");
                failures.push(format!("{place}: {first}"));
            }
            Expect::Rejected(code) if !report.contains(&format!("error[{code}]")) => {
                failures.push(format!("{place}: expected {code}, got: {}", report.trim()));
            }
            _ => {}
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);
    assert!(checked > 0, "no fences found under docs_src");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
