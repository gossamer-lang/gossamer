#![allow(missing_docs)]

//! `gos bindgen --c`: the declarations it writes for a fixture header match
//! the committed golden file, and a program using them - with the C
//! implementation built from `[native]` sources - agrees with C about every
//! layout and calls every function, on the bytecode VM, the JIT, and a
//! native build.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::{TIERS, Tier};

fn gos() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gos"))
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/bindgen_c")
        .join(name)
}

fn bindgen(extra: &[&str]) -> String {
    let out = Command::new(gos())
        .arg("bindgen")
        .arg("--c")
        .arg(fixture("shapes.h"))
        .args(extra)
        .output()
        .expect("run gos bindgen");
    assert!(
        out.status.success(),
        "gos bindgen --c failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n")
}

#[test]
fn the_declarations_match_the_golden_file() {
    let golden = std::fs::read_to_string(fixture("shapes.gos"))
        .expect("read the golden file")
        .replace("\r\n", "\n");
    assert_eq!(bindgen(&["--link", "shapes"]), golden);
}

#[test]
fn an_allow_list_keeps_the_named_declarations_and_what_they_reach() {
    let text = bindgen(&["--allow", "shapes_area"]);
    assert!(
        text.contains("pub fn shapes_area(box: shapes_box) -> i32"),
        "{text}"
    );
    assert!(text.contains("pub struct shapes_box {"), "{text}");
    assert!(text.contains("pub struct shapes_point {"), "{text}");
    assert!(!text.contains("shapes_open"), "{text}");
}

#[test]
fn declarations_that_differ_between_targets_carry_a_cfg() {
    let text = bindgen(&[
        "--target",
        "x86_64-unknown-linux-gnu",
        "--target",
        "x86_64-pc-windows-msvc",
    ]);
    // `size_t` is `unsigned long` on one and `unsigned long long` on the
    // other, but both read as `ffi::size_t`; `struct shapes_point` agrees.
    assert_eq!(text.matches("struct shapes_point {").count(), 1, "{text}");
}

fn run(tier: Tier, dir: &Path) -> String {
    let out = match tier {
        Tier::Bytecode | Tier::Vm => {
            let mut cmd = Command::new(gos());
            cmd.current_dir(dir).args(["run", "src/main.gos"]);
            if tier == Tier::Bytecode {
                cmd.env("GOS_JIT", "0");
            } else {
                cmd.env("GOSSAMER_JIT_THRESHOLD", "1");
            }
            cmd.output().expect("run gos")
        }
        Tier::Llvm => {
            let built = Command::new(gos())
                .current_dir(dir)
                .args(["build", "--release", "--out-dir", "out", "src/main.gos"])
                .output()
                .expect("run gos build");
            assert!(
                built.status.success(),
                "{}",
                String::from_utf8_lossy(&built.stderr)
            );
            Command::new(
                dir.join("out")
                    .join(format!("shapes_use{}", std::env::consts::EXE_SUFFIX)),
            )
            .output()
            .expect("run the binary")
        }
    };
    assert!(
        out.status.success(),
        "{tier:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn a_program_built_on_the_declarations_agrees_with_c() {
    let dir = std::env::temp_dir().join(format!("gos-bindgen-c-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).expect("create the project");
    std::fs::create_dir_all(dir.join("csrc")).expect("create csrc");
    std::fs::copy(fixture("shapes.h"), dir.join("csrc/shapes.h")).expect("copy the header");
    std::fs::copy(fixture("shapes.c"), dir.join("csrc/shapes.c")).expect("copy the source");
    std::fs::write(
        dir.join("project.toml"),
        "[project]\nid = \"example.com/shapes_use\"\nversion = \"0.1.0\"\n\n\
         [native]\nsources = [\"csrc/shapes.c\"]\n",
    )
    .expect("write the manifest");
    std::fs::write(dir.join("src/shapes.gos"), bindgen(&[])).expect("write the bindings");
    std::fs::write(
        dir.join("src/main.gos"),
        r#"use std::ffi
use shapes

unsafe extern "C" {
    fn shapes_label_size() -> ffi::size_t
    fn shapes_label_weight_offset() -> ffi::size_t
    fn shapes_box_size() -> ffi::size_t
}

fn visit(context: Option<ffi::Ptr<ffi::c_void>>, index: i32) -> ffi::c_int {
    index * 10
}

fn main() {
    let label_size = unsafe { shapes_label_size() } as i64
    let weight = unsafe { shapes_label_weight_offset() } as i64
    let box_size = unsafe { shapes_box_size() } as i64
    let size_agrees = ffi::size_of::<shapes::shapes_label>() == label_size
    let offset_agrees = ffi::offset_of::<shapes::shapes_label>("weight") == weight
    println(f"label {size_agrees} {offset_agrees}")
    println(f"box {ffi::size_of::<shapes::shapes_box>() == box_size}")
    let b = shapes::shapes_box { min: shapes::shapes_point { x: 0, y: 0 }, max: shapes::shapes_point { x: 3, y: 4 } }
    let grown = unsafe { shapes::shapes_grow(b, 1) }
    println(f"area {unsafe { shapes::shapes_area(b) }} grown {unsafe { shapes::shapes_area(grown) }}")
    let name = ffi::cstring("registry").unwrap()
    let registry = unsafe { shapes::shapes_open(name, 4) }
    println(f"name {unsafe { shapes::shapes_name_len(registry) }} created {unsafe { ffi::read(ffi::addr_of(shapes::shapes_created)) }}")
    println(f"each {unsafe { shapes::shapes_each(registry, visit, None) }}")
    let value = shapes::shapes_value::new(2.5)
    println(f"real {unsafe { shapes::shapes_value_real(value) }}")
    unsafe { shapes::shapes_close(registry) }
    println(f"{shapes::SHAPES_VERSION} {shapes::SHAPES_NAME} {shapes::SHAPES_FLAG} {shapes::SHAPES_SQUARE}")
}
"#,
    )
    .expect("write the program");
    let expected = run(Tier::Bytecode, &dir);
    assert_eq!(
        expected,
        "label true true\nbox true\narea 12 grown 30\nname 8 created 1\neach 60\nreal 2.5\n3 shapes 16 4\n"
    );
    for tier in TIERS {
        assert_eq!(run(tier, &dir), expected, "{tier:?}");
    }
}
