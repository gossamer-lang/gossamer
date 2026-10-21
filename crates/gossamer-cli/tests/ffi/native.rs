//! `[native]` sources: C a package carries, compiled with the target's C
//! compiler and reached by declarations with no `#[link]`, on every tier.

use std::path::Path;

use crate::support::{Project, TIERS};

const SHIM: &str = r#"#include <stdio.h>
#include <string.h>
#include "scale.h"
#ifndef _WIN32
#include <sys/wait.h>
#endif

/* The digits of `value` as `snprintf` writes them, summed. */
int shim_digit_sum(int value) {
    char text[32];
    int len = snprintf(text, sizeof text, "%d", value);
    int sum = 0;
    for (int i = 0; i < len; i++) {
        if (text[i] >= '0' && text[i] <= '9') sum += text[i] - '0';
    }
    return sum;
}

/* The exit code a `waitpid` status holds, through the `<sys/wait.h>` macros
   where they exist and the same encoding by hand elsewhere. */
int shim_exit_code(int status) {
#ifdef _WIN32
    return (status >> 8) & 0xff;
#else
    return WIFEXITED(status) ? WEXITSTATUS(status) : -1;
#endif
}

int shim_scale(int x) { return x * SCALE * EXTRA; }
"#;

const DECLS: &str = r#"use std::ffi

unsafe extern "C" {
    fn shim_digit_sum(value: ffi::c_int) -> ffi::c_int
    fn shim_exit_code(status: ffi::c_int) -> ffi::c_int
    fn shim_scale(x: ffi::c_int) -> ffi::c_int
}
"#;

const NATIVE_TABLE: &str = r#"
[native]
sources = ["csrc/shim.c"]
include = ["csrc/include"]
defines = { EXTRA = "2" }
"#;

fn write_shim(root: &Path, scale: i32) {
    std::fs::create_dir_all(root.join("csrc/include")).expect("create csrc");
    std::fs::write(root.join("csrc/shim.c"), SHIM).expect("write the shim");
    std::fs::write(
        root.join("csrc/include/scale.h"),
        format!("#define SCALE {scale}\n"),
    )
    .expect("write the header");
}

#[test]
fn a_package_compiles_its_own_c_and_calls_it_on_every_tier() {
    let project = Project::with_manifest(
        "native-own",
        &format!(
            "{DECLS}{}",
            r#"
fn main() {
    let sum = unsafe { shim_digit_sum(90817) }
    let code = unsafe { shim_exit_code(3 << 8) }
    let scaled = unsafe { shim_scale(5) }
    println(f"{sum} {code} {scaled}")
}
"#
        ),
        "",
    );
    let manifest = project.dir.join("project.toml");
    let text = std::fs::read_to_string(&manifest).expect("read the manifest");
    std::fs::write(&manifest, format!("{text}{NATIVE_TABLE}")).expect("extend the manifest");
    write_shim(&project.dir, 3);
    project.expect_everywhere("25 3 30");
    // The object depends on the header it included: a new header is a new
    // answer on every tier.
    write_shim(&project.dir, 4);
    project.expect_everywhere("25 3 40");
}

#[test]
fn a_dependency_brings_its_c_along() {
    let project = Project::new(
        "native-dep",
        r#"use shim

fn main() {
    println(f"{shim::digits(1234)} {shim::scaled(7)}")
}
"#,
    );
    let dep = project.dir.join("deps/shim");
    std::fs::create_dir_all(dep.join("src")).expect("create the dependency");
    std::fs::write(
        dep.join("project.toml"),
        format!("[project]\nid = \"example.com/shim\"\nversion = \"0.1.0\"\n{NATIVE_TABLE}"),
    )
    .expect("write the dependency manifest");
    std::fs::write(
        dep.join("src/lib.gos"),
        format!(
            "{DECLS}{}",
            r"
pub fn digits(n: i32) -> i32 {
    unsafe { shim_digit_sum(n) }
}

pub fn scaled(n: i32) -> i32 {
    unsafe { shim_scale(n) }
}
"
        ),
    )
    .expect("write the dependency");
    write_shim(&dep, 5);
    let manifest = project.dir.join("project.toml");
    let text = std::fs::read_to_string(&manifest).expect("read the manifest");
    std::fs::write(
        &manifest,
        format!("{text}\n[dependencies]\n\"example.com/shim\" = {{ path = \"deps/shim\" }}\n"),
    )
    .expect("extend the manifest");
    for tier in TIERS {
        let out = project.run_on(tier);
        assert!(out.ok, "{tier:?} failed:\n{}", out.all());
        assert_eq!(out.stdout.trim_end(), "10 70", "{tier:?}");
    }
}

#[test]
fn a_native_source_that_does_not_compile_reports_the_compiler() {
    let project = Project::with_manifest(
        "native-broken",
        &format!(
            "{DECLS}{}",
            "\nfn main() {\n    println(unsafe { shim_scale(1) })\n}\n"
        ),
        "",
    );
    let manifest = project.dir.join("project.toml");
    let text = std::fs::read_to_string(&manifest).expect("read the manifest");
    std::fs::write(&manifest, format!("{text}{NATIVE_TABLE}")).expect("extend the manifest");
    write_shim(&project.dir, 3);
    std::fs::write(
        project.dir.join("csrc/shim.c"),
        "int shim_scale(int x) { return x +; }\n",
    )
    .expect("break the shim");
    let out = project.gos(&["run", "src/main.gos"]);
    assert!(!out.ok, "a broken shim ran:\n{}", out.all());
    assert!(
        out.all().contains("compiling the [native] source"),
        "{}",
        out.all()
    );
}

#[test]
fn one_extern_block_links_every_library_it_names() {
    let project = Project::new(
        "two-libraries",
        r#"use std::ffi

#[link(name = "gosffi", search = "native")]
#[link(name = "gosffi2", search = "native")]
unsafe extern "C" {
    fn counter_find(key: ffi::c_int) -> Option<ffi::Ptr<ffi::c_void>>
    fn second_library_answer() -> ffi::c_int
}

fn main() {
    println(f"{unsafe { counter_find(1) }.is_some()} {unsafe { second_library_answer() }}")
}
"#,
    );
    project.expect_everywhere("true 7");
}

#[test]
fn the_jit_leaves_a_function_naming_an_undefined_symbol_to_the_interpreter() {
    let project = Project::new(
        "undefined-symbol",
        r#"use std::ffi

#[link(name = "gosffi", search = "native")]
unsafe extern "C" {
    fn counter_find(key: ffi::c_int) -> Option<ffi::Ptr<ffi::c_void>>
    fn no_library_defines_this() -> ffi::c_int
}

fn hot(n: i64) -> i64 {
    let mut found = 0
    for i in 0..n {
        if unsafe { counter_find(1) }.is_some() { found += 1 }
        if i < 0 { found += unsafe { no_library_defines_this() } as i64 }
    }
    found
}

fn main() {
    println(hot(3))
}
"#,
    );
    let out = project.run_on(crate::support::Tier::Jit);
    assert!(out.ok, "{}", out.all());
    assert_eq!(out.stdout.trim_end(), "3");
}
