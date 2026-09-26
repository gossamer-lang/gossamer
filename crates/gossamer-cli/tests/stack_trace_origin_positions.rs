//! A frame inside a module assembled into the program is reported at that
//! module's own file and line, on the VM and in a debug build.

use std::path::PathBuf;
use std::process::Command;

fn gos_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gos"))
}

#[test]
fn a_panic_in_a_sibling_module_names_that_file_and_line() {
    let dir = std::env::temp_dir().join(format!("gos-origin-frames-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).expect("create project");
    std::fs::write(
        dir.join("project.toml"),
        "[project]\nid = \"example.com/frames\"\nversion = \"0.1.0\"\nentry = \"src/main.gos\"\n",
    )
    .expect("write manifest");
    std::fs::write(
        dir.join("src").join("main.gos"),
        "use util\n\nfn main() {\n    println(\"start\")\n    let v = util::pick(#[1, 2, 3], 7)\n    println(\"unreachable {}\", v)\n}\n",
    )
    .expect("write main");
    std::fs::write(
        dir.join("src").join("util.gos"),
        "// helpers\n\npub fn pick(xs: Vec<i64>, i: i64) -> i64 {\n    let doubled = i * 2\n    xs[doubled]\n}\n",
    )
    .expect("write util");

    let vm = Command::new(gos_bin())
        .current_dir(&dir)
        .env("GOSSAMER_CACHE_DIR", dir.join("cache"))
        .args(["run", "."])
        .output()
        .expect("gos run");
    let vm_stderr = String::from_utf8_lossy(&vm.stderr).into_owned();
    assert!(
        vm_stderr.contains("util::pick") && vm_stderr.contains("util.gos:5"),
        "{vm_stderr}"
    );

    let build = Command::new(gos_bin())
        .current_dir(&dir)
        .env("GOSSAMER_CACHE_DIR", dir.join("cache"))
        .args(["build"])
        .output()
        .expect("gos build");
    let stdout = String::from_utf8_lossy(&build.stdout).into_owned();
    assert!(
        build.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let binary = stdout
        .lines()
        .find_map(|line| {
            let (_, rest) = line.split_once("native executable at ")?;
            Some(PathBuf::from(rest.split(" (target").next()?.trim()))
        })
        .unwrap_or_else(|| panic!("no artifact path in: {stdout}"));
    let native = Command::new(&binary).output().expect("run the artifact");
    let native_stderr = String::from_utf8_lossy(&native.stderr).into_owned();
    assert!(
        native_stderr.contains("  call stack (outermost first):\n    at main (main.gos:5:13)\n    at util::pick (util.gos:5:5)\n"),
        "{native_stderr}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
