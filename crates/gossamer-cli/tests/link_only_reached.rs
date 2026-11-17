//! A native executable links the runtime subsystems its program can reach:
//! the scheduler only with goroutines, the cycle collector only with a type a
//! cycle can pass through, and map code only with a map.

use std::path::{Path, PathBuf};
use std::process::Command;

use object::{Object, ObjectSymbol};

fn gos_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gos"))
}

/// The symbol names of `source` built with `-g`, which keeps the whole symbol
/// table, after checking that it prints `expected`. A mangled Rust name holds
/// each path segment verbatim, so a segment is found by substring.
fn linked_functions(dir: &Path, name: &str, source: &str, expected: &str) -> Vec<String> {
    std::fs::write(dir.join(format!("{name}.gos")), source).expect("write program");
    let build = Command::new(gos_bin())
        .current_dir(dir)
        .env("GOSSAMER_CACHE_DIR", dir.join("cache"))
        .args([
            "build",
            "--release",
            "-g",
            &format!("{name}.gos"),
            "--out-dir",
            "out",
        ])
        .output()
        .expect("spawn gos build");
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let binary = dir.join("out").join(if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    });
    let run = Command::new(&binary).output().expect("run the artifact");
    assert_eq!(String::from_utf8_lossy(&run.stdout), expected);
    let data = std::fs::read(&binary).expect("read the artifact");
    let file = object::File::parse(&*data).expect("parse the artifact");
    file.symbols()
        .filter_map(|s| s.name().ok().map(str::to_string))
        .collect()
}

fn links(functions: &[String], needle: &str) -> bool {
    functions.iter().any(|f| f.contains(needle))
}

#[test]
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn a_program_links_only_the_subsystems_it_reaches() {
    let dir = std::env::temp_dir().join(format!("gos-link-reached-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");

    let hello = linked_functions(
        &dir,
        "hello",
        "fn main() {\n    let mut words = #[\"a\", \"b\"]\n    words.push(\"c\")\n    println(words.len())\n}\n",
        "3\n",
    );
    for absent in [
        "MultiScheduler",
        "blocking_pool",
        "collect_cycles_budgeted",
        "clone_map_storage",
    ] {
        assert!(!links(&hello, absent), "a plain program links {absent}");
    }

    let cyclic = linked_functions(
        &dir,
        "cyclic",
        "struct Node { id: i64, next: Option<Node> }\n\nfn main() {\n    let mut a = Node { id: 1, next: None }\n    let b = Node { id: 2, next: None }\n    a.next = Some(b)\n    println(a.id)\n}\n",
        "1\n",
    );
    assert!(links(&cyclic, "collect_cycles_budgeted"));

    let spawning = linked_functions(
        &dir,
        "spawning",
        "fn main() {\n    let h = spawn(|| 2)\n    println(h.join().unwrap_or(0))\n}\n",
        "2\n",
    );
    assert!(links(&spawning, "MultiScheduler"));

    let mapping = linked_functions(
        &dir,
        "mapping",
        "fn main() {\n    let mut rows = #[{\"a\": 1}]\n    rows.push({\"b\": 2})\n    println(rows.len())\n}\n",
        "2\n",
    );
    assert!(links(&mapping, "clone_map_storage"));

    let _ = std::fs::remove_dir_all(&dir);
}
