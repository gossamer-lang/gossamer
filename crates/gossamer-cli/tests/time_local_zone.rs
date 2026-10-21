#![allow(missing_docs)]

use std::path::PathBuf;
use std::process::Command;

fn gos_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gos"))
}

const PROGRAM: &str = r#"
use std::time

fn main() {
    let here = time::Location::local()
    let named = time::Location::lookup("Local").unwrap()
    let c = here.civil(1700000000000).unwrap()
    println(f"{here.name()} {named.name() == here.name()} {c.hour} {c.offset_seconds}")
}
"#;

#[test]
fn the_local_location_follows_tz_on_the_vm_and_compiled() {
    let dir = std::env::temp_dir().join(format!("gos-local-zone-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("main.gos");
    std::fs::write(&source, PROGRAM).unwrap();
    for (tz, expected) in [
        ("Asia/Tokyo", "Asia/Tokyo true 7 32400"),
        (":America/New_York", "America/New_York true 17 -18000"),
        (
            "/usr/share/zoneinfo/Europe/Paris",
            "Europe/Paris true 23 3600",
        ),
    ] {
        let vm = Command::new(gos_bin())
            .arg("run")
            .arg(&source)
            .env("GOS_JIT", "0")
            .env("TZ", tz)
            .output()
            .expect("run gos");
        assert_eq!(
            String::from_utf8_lossy(&vm.stdout).trim(),
            expected,
            "VM, TZ={tz}"
        );
    }
    let built = Command::new(gos_bin())
        .arg("build")
        .arg(&source)
        .arg("--out-dir")
        .arg(&dir)
        .output()
        .expect("build");
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let native = Command::new(dir.join(format!("main{}", std::env::consts::EXE_SUFFIX)))
        .env("TZ", "Asia/Tokyo")
        .output()
        .expect("run native");
    assert_eq!(
        String::from_utf8_lossy(&native.stdout).trim(),
        "Asia/Tokyo true 7 32400"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
