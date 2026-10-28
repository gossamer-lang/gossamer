fn pkg_workdir(tag: &str) -> PathBuf {
    let mut dir = env::temp_dir();
    dir.push(format!("gos-pkg-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir workdir");
    dir
}

#[test]
fn init_creates_project_toml_with_supplied_id() {
    let dir = pkg_workdir("init");
    let out = Command::new(gos_bin())
        .args(["init", "example.com/widget"])
        .current_dir(&dir)
        .output()
        .expect("spawn init");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let manifest = std::fs::read_to_string(dir.join("project.toml")).unwrap();
    assert!(manifest.contains("example.com/widget"));
    assert!(manifest.contains("0.1.0"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn new_scaffolds_project_directory() {
    let dir = pkg_workdir("new");
    let out = Command::new(gos_bin())
        .args(["new", "example.com/widget", "--path"])
        .arg(dir.join("widget"))
        .output()
        .expect("spawn new");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let project = dir.join("widget");
    assert!(project.join("project.toml").exists());
    assert!(project.join("src/main.gos").exists());
    let main = std::fs::read_to_string(project.join("src/main.gos")).unwrap();
    assert!(main.contains("hello from widget"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn new_service_template_scaffolds_http_handler() {
    let dir = pkg_workdir("new-svc");
    let out = Command::new(gos_bin())
        .args(["new", "example.com/svc", "--template", "service", "--path"])
        .arg(dir.join("svc"))
        .output()
        .expect("spawn new --template service");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let project = dir.join("svc");
    assert!(project.join("project.toml").exists());
    let main = std::fs::read_to_string(project.join("src/main.gos")).unwrap();
    assert!(
        main.contains("http::Handler") && main.contains("http::serve"),
        "service template missing http wiring:\n{main}"
    );
    assert!(
        !project.join("src/lib.gos").exists(),
        "service template should not emit lib.gos"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn new_rejects_unknown_template() {
    let dir = pkg_workdir("new-bad");
    let out = Command::new(gos_bin())
        .args(["new", "example.com/bad", "--template", "nope", "--path"])
        .arg(dir.join("bad"))
        .output()
        .expect("spawn new --template nope");
    assert!(
        !out.status.success(),
        "clap should reject unknown template values"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn add_and_remove_round_trip_a_dependency() {
    let dir = pkg_workdir("addrm");
    let init = Command::new(gos_bin())
        .args(["init", "example.com/widget"])
        .current_dir(&dir)
        .output()
        .expect("init");
    assert!(init.status.success());
    let add = Command::new(gos_bin())
        .args(["add", "example.org/lib@1.2.3"])
        .current_dir(&dir)
        .output()
        .expect("add");
    assert!(
        add.status.success(),
        "{}",
        String::from_utf8_lossy(&add.stderr)
    );
    let after_add = std::fs::read_to_string(dir.join("project.toml")).unwrap();
    assert!(after_add.contains("\"example.org/lib\" = \"1.2.3\""));
    let remove = Command::new(gos_bin())
        .args(["remove", "example.org/lib"])
        .current_dir(&dir)
        .output()
        .expect("remove");
    assert!(remove.status.success());
    let after_remove = std::fs::read_to_string(dir.join("project.toml")).unwrap();
    assert!(!after_remove.contains("example.org/lib"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn tidy_canonicalises_existing_manifest() {
    let dir = pkg_workdir("tidy");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("project.toml"),
        "[project]\nid = \"example.com/widget\"\nversion = \"0.1.0\"\n\n[dependencies]\n\"example.org/lib\" = \"1.0.0\"\n",
    )
    .unwrap();
    let out = Command::new(gos_bin())
        .arg("tidy")
        .current_dir(&dir)
        .output()
        .expect("tidy");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let after = std::fs::read_to_string(dir.join("project.toml")).unwrap();
    assert!(after.contains("example.org/lib"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn run_refuses_type_invalid_program_with_diagnostic() {
    // The VM must not execute programs that fail static checks
    // (error_handling.md type-safety invariant). The CLI should print a
    // typed diagnostic and exit non-zero.
    let fixture = write_fixture(
        "type-fail",
        "fn main() -> i64 {\n    let x: i64 = \"not an int\"\n    x\n}\n",
    );
    let out = Command::new(gos_bin())
        .arg("run")
        .arg(&fixture)
        .output()
        .expect("spawn gos");
    assert!(
        !out.status.success(),
        "run should reject type-invalid source; stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("type") && stderr.contains("refusing to execute"),
        "expected typed diagnostic + refusal; got: {stderr}"
    );
    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn test_refuses_statically_invalid_program_with_diagnostic() {
    // Same invariant applies to `gos test`: a test harness that runs
    // statically-broken code is worse than useless. Put the test at
    // top level so name resolution fires before the tree-walker sees
    // it (nested-module resolution is tracked separately).
    let fixture = write_fixture(
        "test-unresolved",
        "#[test]\nfn has_unresolved_name() {\n    totally_made_up_fn()\n}\n",
    );
    let out = Command::new(gos_bin())
        .args(["test"])
        .arg(&fixture)
        .output()
        .expect("spawn gos test");
    assert!(
        !out.status.success(),
        "test should reject type-invalid source; stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("refusing to execute"),
        "expected static-error refusal in stderr: {stderr}"
    );
    let _ = std::fs::remove_file(&fixture);
}

// Post-L4 there's no launcher path - the old
// `unsupported_native_path_fails_loudly_by_default` /
// `allow_launcher_emits_shell_launcher_when_codegen_bails` tests
// exercised a flag that no longer exists. Every program the
// resolver + typechecker accepts now lowers to a native binary;
// a codegen bail is a compiler bug, not an expected path.

#[test]
fn explain_recognises_runtime_error_codes() {
    // `gos explain GX0005` must print the long-form panic
    // explanation so the runtime-error catalogue stays in sync with
    // the `RuntimeError::code` method in `gossamer-interp`.
    // (parity_error_plan.md Phase E4).
    let out = Command::new(gos_bin())
        .args(["explain", "GX0005"])
        .output()
        .expect("spawn gos explain");
    assert!(
        out.status.success(),
        "explain should succeed; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("GX0005") && stdout.to_lowercase().contains("panic"),
        "expected panic explanation referencing GX0005; got: {stdout}"
    );
}

#[test]
fn runtime_panic_stderr_carries_gx_code_prefix() {
    // Unified error-code catalogue: every runtime failure's stderr
    // is prefixed with `error[GXNNNN]:`. An explicit `panic!(...)`
    // exercises the `GX0005` branch end-to-end.
    let fixture = write_fixture("runtime-panic", "fn main() {\n    panic(\"boom\")\n}\n");
    let out = Command::new(gos_bin())
        .arg("run")
        .arg(&fixture)
        .output()
        .expect("spawn gos");
    assert!(!out.status.success(), "panic should exit non-zero");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("error[GX0005]"),
        "expected GX0005 prefix in stderr: {stderr}"
    );
    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn test_subcommand_with_no_args_walks_up_to_project_toml() {
    // `gos test` with no path argument should locate the nearest
    // ancestor `project.toml` and discover every `.gos` file under
    // its `src/` tree - mimicking `cargo test` ergonomics.
    let dir = pkg_workdir("test-default");
    let init = Command::new(gos_bin())
        .args(["init", "example.com/svc"])
        .current_dir(&dir)
        .output()
        .expect("spawn init");
    assert!(
        init.status.success(),
        "init: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    let src = dir.join("src");
    std::fs::create_dir_all(&src).expect("mkdir src");
    std::fs::write(
        src.join("main.gos"),
        "use std::testing\n\
         fn add(a: i64, b: i64) -> i64 { a + b }\n\
         #[cfg(test)]\n\
         mod tests {\n\
         \x20\x20\x20\x20use std::testing\n\
         \x20\x20\x20\x20#[test]\n\
         \x20\x20\x20\x20fn add_combines_two_ints() {\n\
         \x20\x20\x20\x20\x20\x20\x20\x20let _ = testing::check_eq(super::add(2, 3), 5, \"add\")\n\
         \x20\x20\x20\x20}\n\
         }\n\
         fn main() { }\n",
    )
    .expect("write src/main.gos");
    let nested = src.join("inner");
    std::fs::create_dir_all(&nested).expect("mkdir inner");
    let cwd = nested;
    let out = Command::new(gos_bin())
        .arg("test")
        .current_dir(&cwd)
        .output()
        .expect("spawn test");
    assert!(
        out.status.success(),
        "stderr: {}\nstdout: {}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("add_combines_two_ints"),
        "expected discovered test name in output: {stdout}"
    );
    assert!(
        stdout.contains("1 passed"),
        "expected pass tally in output: {stdout}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn examples_web_service_project_tests_all_pass() {
    // The `examples/projects/web_service` project is the canonical
    // multi-endpoint Gossamer service. Its render-helper unit tests
    // double as a smoke test that `gos test` (no args) discovers and
    // runs the project's full `src/` tree.
    let project = examples_dir().join("projects").join("web_service");
    assert!(
        project.join("project.toml").is_file(),
        "missing project.toml at {}",
        project.display()
    );
    let out = Command::new(gos_bin())
        .arg("test")
        .current_dir(&project)
        .output()
        .expect("spawn test");
    assert!(
        out.status.success(),
        "stderr: {}\nstdout: {}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    for tname in [
        "health_returns_ok",
        "users_returns_json_list_with_known_names",
        "echo_wraps_query_in_json",
        "echo_handles_empty_query",
        "classify_routes_known_paths",
        "classify_falls_back_to_not_found",
    ] {
        assert!(
            stdout.contains(tname),
            "missing test {tname} in output:\n{stdout}"
        );
    }
    assert!(
        stdout.contains("6 passed") && stdout.contains("0 failed"),
        "expected full pass tally; stdout was:\n{stdout}"
    );
}

#[test]
fn examples_rust_binding_add_project_tests_all_pass() {
    // `examples/projects/rust_binding_add` is the canonical
    // minimal Rust-binding example: one `fn add(i64, i64) -> i64`
    // in `addlib/` exposed to Gossamer via `register_module!` and
    // exercised by `#[test]`s in `src/main.gos`. Confirms the
    // end-to-end `[rust-bindings]` wiring works through `gos test`.
    let project = examples_dir().join("projects").join("rust_binding_add");
    assert!(
        project.join("project.toml").is_file(),
        "missing project.toml at {}",
        project.display()
    );
    assert!(
        project.join("addlib").join("Cargo.toml").is_file(),
        "missing addlib/Cargo.toml at {}",
        project.display()
    );
    let out = Command::new(gos_bin())
        .arg("test")
        .current_dir(&project)
        .output()
        .expect("spawn test");
    assert!(
        out.status.success(),
        "stderr: {}\nstdout: {}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    for tname in [
        "add_combines_two_positive_ints",
        "add_handles_zero_identity",
        "add_handles_negative_summands",
    ] {
        assert!(
            stdout.contains(tname),
            "missing test {tname} in output:\n{stdout}"
        );
    }
    assert!(
        stdout.contains("3 passed") && stdout.contains("0 failed"),
        "expected full pass tally; stdout was:\n{stdout}"
    );
}

#[test]
fn examples_rust_binding_shapes_agree_across_tiers() {
    // `examples/projects/rust_binding_shapes` exercises the binding
    // shapes that are converted at the boundary rather than crossing
    // as a bare word: `Bytes`, `Map<K, V>`, a tuple, and a
    // `#[derive(GosStruct)]` struct. The bytecode VM and the native
    // build must print the same lines for each.
    let project = examples_dir().join("projects").join("rust_binding_shapes");
    let tested = Command::new(gos_bin())
        .arg("test")
        .current_dir(&project)
        .output()
        .expect("spawn test");
    assert!(
        tested.status.success(),
        "stderr: {}\nstdout: {}",
        String::from_utf8_lossy(&tested.stderr),
        String::from_utf8_lossy(&tested.stdout)
    );

    let interpreted = Command::new(gos_bin())
        .arg("run")
        .arg("src/main.gos")
        .current_dir(&project)
        .output()
        .expect("spawn run");
    assert!(
        interpreted.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&interpreted.stderr)
    );

    let built = Command::new(gos_bin())
        .arg("build")
        .arg("src/main.gos")
        .current_dir(&project)
        .output()
        .expect("spawn build");
    assert!(
        built.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&built.stderr)
    );
    let binary = project
        .join("target")
        .join("debug")
        .join("rust_binding_shapes");
    let native = Command::new(&binary)
        .current_dir(&project)
        .output()
        .expect("spawn native binary");
    assert!(
        native.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&native.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&interpreted.stdout),
        String::from_utf8_lossy(&native.stdout),
        "bytecode VM and native build disagree on the binding wire shapes"
    );
}

#[test]
fn jit_compiled_binding_call_resolves_predeclared_symbol() {
    // A [rust-bindings] call reached from a JIT-compiled function must
    // resolve its `gos_binding_*` symbol from the intrinsic cache that
    // the pre-declare phase fills. Otherwise the first reference lands in
    // the Cranelift parallel phase, where OfflineModule::declare_function
    // is unreachable, and the gos-vm thread aborts. Forcing an immediate
    // JIT (GOSSAMER_JIT_THRESHOLD=1) over a hot loop drives that path.
    let addlib = examples_dir()
        .join("projects")
        .join("rust_binding_add")
        .join("addlib");
    assert!(
        addlib.join("Cargo.toml").is_file(),
        "missing addlib crate at {}",
        addlib.display()
    );
    let workspace_root = examples_dir()
        .parent()
        .expect("workspace root")
        .to_path_buf();

    let tmp = env::temp_dir().join(format!("gos-jit-binding-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("src")).expect("create src");
    std::fs::write(
        tmp.join("project.toml"),
        format!(
            "[project]\nid = \"example.com/jitbind\"\nversion = \"0.1.0\"\n\n\
             [rust-bindings]\naddlib = {{ path = {addlib:?} }}\n"
        ),
    )
    .expect("write project.toml");
    std::fs::write(
        tmp.join("src").join("main.gos"),
        "use addlib::add\n\
         fn hot(x: i64) -> i64 { add(x, 1) }\n\
         fn main() {\n    \
             let mut total: i64 = 0\n    \
             for i in 0..3000 { total += hot(i) }\n    \
             println(\"total = {}\", total)\n\
         }\n",
    )
    .expect("write main.gos");

    let out = Command::new(gos_bin())
        .arg("run")
        .arg("src/main.gos")
        .current_dir(&tmp)
        .env("GOSSAMER_ROOT", &workspace_root)
        .env("GOSSAMER_CACHE", tmp.join("cache"))
        .env("GOSSAMER_JIT_THRESHOLD", "1")
        .output()
        .expect("spawn run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let _ = std::fs::remove_dir_all(&tmp);
    assert!(
        out.status.success(),
        "gos aborted on a JIT-compiled binding call\nstdout: {stdout}\nstderr: {stderr}"
    );
    // sum_{i=0}^{2999} add(i, 1) == sum(1..=3000) == 3000 * 3001 / 2.
    assert!(
        stdout.contains("total = 4501500"),
        "unexpected output:\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
fn binding_build_ignores_cargo_config_of_the_invoking_directory() {
    // A library checkout keeps a `.cargo/config.toml` for its own crates, such
    // as a `[patch]` to a sibling checkout. The project gos generates for the
    // bindings is built under its own configuration, wherever gos was started.
    let addlib = examples_dir()
        .join("projects")
        .join("rust_binding_add")
        .join("addlib");
    let workspace_root = examples_dir()
        .parent()
        .expect("workspace root")
        .to_path_buf();

    let tmp = env::temp_dir().join(format!("gos-binding-cargo-config-{}", std::process::id()));
    // The cache sits beside the project, as the user cache does, so only the
    // working directory reaches the project's cargo config.
    let cache = env::temp_dir().join(format!(
        "gos-binding-cargo-config-cache-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let _ = std::fs::remove_dir_all(&cache);
    std::fs::create_dir_all(tmp.join("src")).expect("create src");
    std::fs::create_dir_all(tmp.join(".cargo")).expect("create .cargo");
    std::fs::write(
        tmp.join(".cargo").join("config.toml"),
        format!(
            "[patch.crates-io]\ngossamer-binding = {{ path = {:?} }}\n",
            tmp.join("no-such-checkout")
        ),
    )
    .expect("write .cargo/config.toml");
    std::fs::write(
        tmp.join("project.toml"),
        format!(
            "[project]\nid = \"example.com/cargoconfig\"\nversion = \"0.1.0\"\n\n\
             [rust-bindings]\naddlib = {{ path = {addlib:?} }}\n"
        ),
    )
    .expect("write project.toml");
    std::fs::write(
        tmp.join("src").join("main.gos"),
        "use addlib::add\nfn main() { println(\"sum = {}\", add(2, 3)) }\n",
    )
    .expect("write main.gos");

    let out = Command::new(gos_bin())
        .arg("run")
        .arg("src/main.gos")
        .current_dir(&tmp)
        .env("GOSSAMER_ROOT", &workspace_root)
        .env("GOSSAMER_CACHE", &cache)
        .output()
        .expect("spawn run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let _ = std::fs::remove_dir_all(&tmp);
    let _ = std::fs::remove_dir_all(&cache);
    assert!(
        out.status.success(),
        "the binding build read the invoking directory's cargo config\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(stdout.contains("sum = 5"), "stdout: {stdout}\nstderr: {stderr}");
}

#[test]
fn run_main_thread_flag_executes_program() {
    // `gos run --main-thread` runs the VM on the process main thread
    // (for native libraries that require it) instead of the spawned
    // `gos-vm` thread. The program must still execute correctly.
    let fixture = write_fixture("main-thread", "fn main() { println(\"mt {}\", 40 + 2) }\n");
    let out = Command::new(gos_bin())
        .arg("run")
        .arg("--main-thread")
        .arg(&fixture)
        .output()
        .expect("spawn run --main-thread");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let _ = std::fs::remove_file(&fixture);
    assert!(out.status.success(), "stderr: {stderr}");
    assert!(stdout.contains("mt 42"), "unexpected output: {stdout}");
}

#[test]
fn skill_prompt_subcommand_prints_skill_card() {
    let out = Command::new(gos_bin())
        .arg("skill-prompt")
        .output()
        .expect("spawn skill-prompt");
    assert!(out.status.success(), "skill-prompt should exit zero");
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    assert!(
        stdout.starts_with("# Gossamer"),
        "skill card should start with the title: {}",
        stdout.lines().next().unwrap_or("")
    );
    assert!(
        stdout.contains("|>"),
        "skill card should mention the forward-pipe operator"
    );
    assert!(
        stdout.contains("Goroutines"),
        "skill card should cover concurrency"
    );
}

// --- N6: must_use Result lint (SPEC §9) ---

#[test]
fn discarded_result_is_a_type_error() {
    // SPEC §9: a `Result<T, E>` value used as a statement without
    // binding or propagating the result is a compile error (GT0007).
    // `gos` must refuse to execute and mention the error code.
    // `let _ = expr` is the explicit-discard exception and must NOT
    // trigger the diagnostic.
    let src = r#"
use std::errors

fn may_fail(n: i64) -> Result<i64, errors::Error> {
    if n > 0 { Ok(n) } else { Err(errors::new("negative")) }
}

fn main() {
    may_fail(1)
}
"#;
    let fixture = write_fixture("n6-discard-result", src);
    let out = std::process::Command::new(gos_bin())
        .arg("run")
        .arg(&fixture)
        .output()
        .expect("spawn gos");
    let _ = std::fs::remove_file(&fixture);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "run must reject a discarded Result; stderr: {stderr}"
    );
    assert!(
        stderr.contains("GT0007")
            || stderr.contains("unused `Result`")
            || stderr.contains("Result"),
        "expected GT0007 or a Result-related diagnostic in stderr; got: {stderr}"
    );
}

#[test]
fn let_underscore_result_is_not_an_error() {
    // `let _ = expr` is the explicit-discard form for Result. It must
    // NOT trigger GT0007 - the user has consciously chosen to ignore
    // the Result (best-effort operations, etc.).
    let src = r#"
use std::errors

fn may_fail(n: i64) -> Result<i64, errors::Error> {
    if n > 0 { Ok(n) } else { Err(errors::new("negative")) }
}

fn main() {
    let _ = may_fail(1)
    println("ok")
}
"#;
    let fixture = write_fixture("n6-let-underscore-ok", src);
    let out = std::process::Command::new(gos_bin())
        .arg("run")
        .arg(&fixture)
        .output()
        .expect("spawn gos");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let _ = std::fs::remove_file(&fixture);
    assert!(
        out.status.success(),
        "let _ = result should be accepted; stderr: {stderr}"
    );
    assert!(
        stdout.contains("ok"),
        "expected 'ok' in stdout; got: {stdout}"
    );
}

#[test]
fn bare_manifest_id_is_a_hard_error_for_project_commands() {
    // A bare `id = "name"` used to silently disable `[rust-bindings]`
    // resolution while `gos check` / `gos test` kept passing. A
    // present-but-malformed manifest must fail loudly instead.
    let dir = env::temp_dir().join(format!("gos-bare-id-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("project.toml"),
        "[project]\nid = \"bareid\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(dir.join("src/main.gos"), "fn main() { println(\"hi\") }\n").unwrap();
    let out = Command::new(gos_bin())
        .arg("test")
        .arg(".")
        .current_dir(&dir)
        .output()
        .expect("spawn gos test");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        !out.status.success(),
        "bare manifest id must fail; stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("invalid domain segment") || stdout.contains("invalid domain segment"),
        "diagnostic must explain the id grammar; stdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
fn unbound_binding_module_import_is_rejected() {
    // `use brotli` with nothing declaring `brotli` names no module, so it
    // is reported where it is written. Binding the name anyway would let
    // a call to it return Unit, and a test suite "pass" with no coverage.
    let dir = env::temp_dir().join(format!("gos-unbound-binding-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("project.toml"),
        "[project]\nid = \"example.com/unbound\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/main.gos"),
        r#"use brotli
use std::testing

fn main() {
    println("unused")
}

#[cfg(test)]
mod tests {
    use std::testing

    #[test]
    fn unbound_decode_is_loud() {
        match brotli::decode([1, 2, 3]) {
            Ok(_) => { let _ = testing::check(false, "must not decode") },
            Err(_) => { let _ = testing::check(true, "error surfaced") },
        }
    }
}
"#,
    )
    .unwrap();
    let out = Command::new(gos_bin())
        .arg("test")
        .arg(".")
        .current_dir(&dir)
        .output()
        .expect("spawn gos test");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        !out.status.success(),
        "unbound binding call must fail the test run; stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("GR0005") && stderr.contains("brotli"),
        "failure must name the unresolved import; stdout: {stdout}\nstderr: {stderr}"
    );
}
