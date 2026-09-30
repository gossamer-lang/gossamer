//! `gos new` and `gos init` leave a project ready for version control: the
//! build output and caches a first `gos build` writes are ignored.

use std::path::PathBuf;
use std::process::Command;

fn gos_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gos"))
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gos-scaffold-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

#[test]
fn every_new_template_ignores_build_output() {
    let root = scratch("new");
    for template in ["bin", "lib", "service", "workspace", "binding"] {
        let dir = root.join(template);
        let output = Command::new(gos_bin())
            .arg("new")
            .arg(format!("example.com/{template}"))
            .arg("--path")
            .arg(&dir)
            .arg("--template")
            .arg(template)
            .output()
            .expect("spawn gos new");
        assert!(
            output.status.success(),
            "gos new --template {template}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let ignore = std::fs::read_to_string(dir.join(".gitignore"))
            .unwrap_or_else(|_| panic!("{template}: no .gitignore"));
        assert!(
            ignore.lines().any(|l| l == "target/"),
            "{template}: {ignore}"
        );
        assert!(
            ignore.lines().any(|l| l == ".gos-cache/"),
            "{template}: {ignore}"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn init_writes_a_gitignore_and_keeps_an_existing_one() {
    let dir = scratch("init");
    let run = |dir: &PathBuf| {
        Command::new(gos_bin())
            .current_dir(dir)
            .arg("init")
            .arg("example.com/fresh")
            .output()
            .expect("spawn gos init")
    };
    assert!(run(&dir).status.success());
    assert!(std::fs::read_to_string(dir.join(".gitignore")).is_ok_and(|t| t.contains("target/")));

    let kept = scratch("init-kept");
    std::fs::write(kept.join(".gitignore"), "mine\n").expect("seed .gitignore");
    assert!(run(&kept).status.success());
    assert_eq!(
        std::fs::read_to_string(kept.join(".gitignore")).expect("read"),
        "mine\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&kept);
}
