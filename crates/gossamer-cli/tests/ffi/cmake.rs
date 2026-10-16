//! `[native.cmake]`: a `CMake` project a package builds for the libraries its
//! `#[link]` declarations name, linked statically into a native build and
//! loaded as a shared library by the bytecode VM and the JIT.

use crate::support::Project;

const CMAKE_LISTS: &str = r"cmake_minimum_required(VERSION 3.16)
project(tally C)
set(CMAKE_WINDOWS_EXPORT_ALL_SYMBOLS ON)
add_library(tally tally.c)
target_compile_definitions(tally PRIVATE TALLY_STEP=${TALLY_STEP})
install(TARGETS tally ARCHIVE DESTINATION lib LIBRARY DESTINATION lib RUNTIME DESTINATION bin)
";

const TALLY: &str = "int tally_step(int x) { return x + TALLY_STEP; }\n";

const PROGRAM: &str = r#"use std::ffi

#[link(name = "tally")]
unsafe extern "C" {
    fn tally_step(x: ffi::c_int) -> ffi::c_int
}

fn main() {
    println(unsafe { tally_step(4) })
}
"#;

const MANIFEST: &str = r#"
[features]
default = ["vendored"]
vendored = []

[native.cmake]
source = "vendor/tally"
provides = ["tally"]
options = { TALLY_STEP = "3" }
feature = "vendored"
"#;

fn project(tag: &str) -> Project {
    let project = Project::with_manifest(tag, PROGRAM, MANIFEST);
    let vendor = project.dir.join("vendor/tally");
    std::fs::create_dir_all(&vendor).expect("create the CMake project");
    std::fs::write(vendor.join("CMakeLists.txt"), CMAKE_LISTS).expect("write CMakeLists.txt");
    std::fs::write(vendor.join("tally.c"), TALLY).expect("write the source");
    project
}

#[test]
fn a_cmake_build_provides_the_library_a_link_names_on_every_tier() {
    project("cmake-provides").expect_everywhere("7");
}

#[test]
fn a_cmake_build_belongs_to_its_feature() {
    let project = project("cmake-feature");
    let out = project.gos(&["run", "--no-default-features", "src/main.gos"]);
    assert!(!out.ok, "the build ran without its feature:\n{}", out.all());
    assert!(
        out.all().contains("tally_step"),
        "expected an unresolved `tally_step`:\n{}",
        out.all()
    );
}

const LAYERED_LISTS: &str = r"cmake_minimum_required(VERSION 3.16)
project(tally C)
set(CMAKE_WINDOWS_EXPORT_ALL_SYMBOLS ON)
set(CMAKE_POSITION_INDEPENDENT_CODE ON)
add_library(stepper STATIC stepper.c)
add_library(tally tally.c)
target_link_libraries(tally PRIVATE stepper)
target_compile_definitions(tally PRIVATE TALLY_STEP=${TALLY_STEP})
configure_file(tally.pc.in tally.pc @ONLY)
install(TARGETS tally stepper ARCHIVE DESTINATION lib LIBRARY DESTINATION lib RUNTIME DESTINATION bin)
install(FILES ${CMAKE_CURRENT_BINARY_DIR}/tally.pc DESTINATION lib/pkgconfig)
";

const LAYERED_TALLY: &str = "int stepper_next(int x);\nint tally_step(int x) { return stepper_next(x) + TALLY_STEP - 1; }\n";

const STEPPER: &str = "int stepper_next(int x) { return x + 1; }\n";

// The static `tally` archive needs `stepper`, which only its pkg-config
// file names.
const TALLY_PC: &str = "prefix=@CMAKE_INSTALL_PREFIX@
libdir=${prefix}/lib

Name: tally
Description: a library over another
Version: 1.0
Libs: -L${libdir} -ltally -lstepper
";

#[test]
fn a_static_cmake_library_links_what_its_pkg_config_file_names() {
    let project = Project::with_manifest("cmake-layered", PROGRAM, MANIFEST);
    let vendor = project.dir.join("vendor/tally");
    std::fs::create_dir_all(&vendor).expect("create the CMake project");
    for (file, text) in [
        ("CMakeLists.txt", LAYERED_LISTS),
        ("tally.c", LAYERED_TALLY),
        ("stepper.c", STEPPER),
        ("tally.pc.in", TALLY_PC),
    ] {
        std::fs::write(vendor.join(file), text).expect("write the CMake project");
    }
    project.expect_everywhere("7");
}
