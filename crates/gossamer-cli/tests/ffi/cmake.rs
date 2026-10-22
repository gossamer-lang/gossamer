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
        !out.all().contains("configuring the CMake project"),
        "the CMake project built without its feature:\n{}",
        out.all()
    );
    assert!(
        out.all().contains("tally"),
        "expected the missing `tally` to be named:\n{}",
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

/// Two projects that depend on one checkout of a `[native.cmake]` package
/// build it once: the second finds the first's build in the per-user cache.
#[test]
fn projects_sharing_a_cmake_package_share_its_build() {
    let root = std::env::temp_dir().join(format!("gos-ffi-cmake-shared-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let lib = root.join("tally-lib");
    let vendor = lib.join("vendor/tally");
    std::fs::create_dir_all(lib.join("src")).expect("create the library");
    std::fs::create_dir_all(&vendor).expect("create the CMake project");
    std::fs::write(
        lib.join("project.toml"),
        "[project]\nid = \"example.com/tally-lib\"\nversion = \"0.1.0\"\n\n\
         [native.cmake]\nsource = \"vendor/tally\"\nprovides = [\"tally\"]\n\
         options = { TALLY_STEP = \"3\" }\n",
    )
    .expect("write the library manifest");
    std::fs::write(
        lib.join("src/lib.gos"),
        "use std::ffi\n\n#[link(name = \"tally\")]\nunsafe extern \"C\" {\n    \
         fn tally_step(x: ffi::c_int) -> ffi::c_int\n}\n\n\
         pub fn step(x: i64) -> i64 {\n    unsafe { tally_step(x as ffi::c_int) } as i64\n}\n",
    )
    .expect("write the library");
    std::fs::write(vendor.join("CMakeLists.txt"), CMAKE_LISTS).expect("write CMakeLists.txt");
    std::fs::write(vendor.join("tally.c"), TALLY).expect("write the source");
    let cache = root.join("cache");
    let run_app = |name: &str| {
        let app = root.join(name);
        std::fs::create_dir_all(app.join("src")).expect("create the app");
        std::fs::write(
            app.join("project.toml"),
            format!(
                "[project]\nid = \"example.com/{name}\"\nversion = \"0.1.0\"\n\n\
                 [dependencies]\n\"example.com/tally-lib\" = {{ path = \"../tally-lib\" }}\n"
            ),
        )
        .expect("write the app manifest");
        std::fs::write(
            app.join("src/main.gos"),
            "use tally_lib\n\nfn main() {\n    println(tally_lib::step(4))\n}\n",
        )
        .expect("write the app");
        let output = std::process::Command::new(crate::support::gos_binary())
            .current_dir(&app)
            .args(["run", "--native", "."])
            .env("XDG_CACHE_HOME", &cache)
            .output()
            .expect("run gos");
        // A run keeps what its build said in the build's log, not in the
        // program's stderr.
        let log =
            std::fs::read_to_string(app.join(".gos-cache/run/debug/build.log")).unwrap_or_default();
        (output, log)
    };
    let (first, first_log) = run_app("first-app");
    let (second, second_log) = run_app("second-app");
    let _ = std::fs::remove_dir_all(&root);
    let first_err = format!("{}{first_log}", String::from_utf8_lossy(&first.stderr));
    let second_err = format!("{}{second_log}", String::from_utf8_lossy(&second.stderr));
    assert_eq!(
        String::from_utf8_lossy(&first.stdout).trim(),
        "7",
        "{first_err}"
    );
    assert_eq!(
        String::from_utf8_lossy(&second.stdout).trim(),
        "7",
        "{second_err}"
    );
    assert!(
        first_err.contains("configuring the CMake project"),
        "the first project builds the package: {first_err}"
    );
    assert!(
        !second_err.contains("configuring the CMake project"),
        "the second project reuses the first's build: {second_err}"
    );
}
