#[test]
fn run_executes_every_terminating_example() {
    // `web_server.gos` is a real server that runs forever by design,
    // so it is not part of this loop. See
    // `web_server_example_binds_and_serves_real_requests` for
    // end-to-end coverage of the server path.
    for name in ["hello_world.gos", "line_count.gos"] {
        let path = examples_dir().join(name);
        let out = Command::new(gos_bin())
            .arg("run")
            .arg(&path)
            .output()
            .expect("spawn run");
        assert!(
            out.status.success(),
            "{name} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// End-to-end smoke test of the echo example. Spawns
/// `gos run examples/web_server.gos` in a child process, connects,
/// drives a real HTTP/1.1 request, and inspects the response.
///
/// The example takes its listen address, so the test hands it one the kernel
/// chose and every answer below is an assertion rather than a skip.
#[test]
fn web_server_example_binds_and_serves_real_requests() {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::thread;
    use std::time::Duration;

    // The kernel names the port and the example is told which one, so this
    // test contends with nothing - not another tier's run, and not whatever a
    // developer left listening on a well-known number.
    let addr = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
        let addr = probe.local_addr().expect("read the bound address");
        drop(probe);
        addr.to_string()
    };

    let mut child = match std::process::Command::new(gos_bin())
        .arg("run")
        .arg(examples_dir().join("web_server.gos"))
        .arg(&addr)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(err) => {
            eprintln!("skipping - could not spawn gos: {err}");
            return;
        }
    };

    let mut response: Option<Vec<u8>> = None;
    for _ in 0..40 {
        thread::sleep(Duration::from_millis(100));
        if let Ok(mut stream) = TcpStream::connect(&addr) {
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let _ =
                stream.write_all(b"GET /echo?name=jane&x=1 HTTP/1.1\r\nHost: localhost\r\n\r\n");
            stream.shutdown(std::net::Shutdown::Write).unwrap();
            let mut buf = Vec::new();
            if stream.read_to_end(&mut buf).is_ok() && !buf.is_empty() {
                response = Some(buf);
                break;
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();

    let body = response.unwrap_or_else(|| panic!("{addr} never answered the echo request"));
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.starts_with("HTTP/1.1 "),
        "response is not HTTP/1.1: {text}"
    );
    assert!(
        text.contains("method") && text.contains("GET"),
        "echo body missing the method: {text}"
    );
    assert!(
        text.contains("query") && text.contains("name=jane"),
        "echo body missing query: {text}"
    );
}

#[test]
fn fmt_rewrites_misformatted_source() {
    let fixture = write_fixture("fmt", "fn    main(  )   {   }\n");
    let out = Command::new(gos_bin())
        .args(["fmt"])
        .arg(&fixture)
        .output()
        .expect("spawn fmt");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let formatted = std::fs::read_to_string(&fixture).unwrap();
    assert!(formatted.starts_with("fn main("));
    assert!(!formatted.contains("    main(  )"));
    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn fmt_check_flag_fails_on_unformatted_file() {
    let fixture = write_fixture("fmtcheck", "fn    main()    {}\n");
    let out = Command::new(gos_bin())
        .args(["fmt", "--check"])
        .arg(&fixture)
        .output()
        .expect("spawn fmt --check");
    assert!(!out.status.success(), "--check should fail on messy input");
    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn doc_lists_items_defined_in_the_file() {
    let fixture = write_fixture("doc", "struct Widget { }\nfn main() { }\nfn helper() { }\n");
    let out = Command::new(gos_bin())
        .args(["doc"])
        .arg(&fixture)
        .output()
        .expect("spawn doc");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("struct Widget"));
    assert!(text.contains("fn main"));
    assert!(text.contains("fn helper"));
    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn test_subcommand_runs_hash_test_attributed_functions() {
    let fixture = write_fixture(
        "testharness",
        "#[test]\nfn test_ok() { println(\"ran-test\"); assert(true, \"ran\") }\nfn main() { }\n",
    );
    let out = Command::new(gos_bin())
        .args(["test"])
        .arg(&fixture)
        .output()
        .expect("spawn test");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("1 passed"));
    assert!(stdout.contains("ran-test"));
    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn test_subcommand_reports_no_tests_when_absent() {
    let fixture = write_fixture("testempty", "fn main() { }\n");
    let out = Command::new(gos_bin())
        .args(["test"])
        .arg(&fixture)
        .output()
        .expect("spawn test");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("no #[test] functions"));
    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn bench_subcommand_reports_ns_per_op() {
    // No-op bench fn - exercises the formatter and the calibration
    // cap on a fn that never crosses the 50ms trial threshold.
    let fixture = write_fixture(
        "benchharness_noop",
        "#[bench]\nfn bench_noop() { }\nfn main() { }\n",
    );
    let out = Command::new(gos_bin())
        .args(["bench"])
        .arg(&fixture)
        .output()
        .expect("spawn bench");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("ns/op"),
        "expected ns/op in stdout, got: {stdout}"
    );
    assert!(
        stdout.contains("tier-ups")
            && stdout.contains("native-code")
            && stdout.contains("peak-rss")
            && stdout.contains("allocs")
            && stdout.contains("arc +")
            && stdout.contains("boundary-copies"),
        "expected JIT benchmark counters in stdout, got: {stdout}"
    );
    assert!(
        stdout.contains("bench_noop"),
        "expected the bench label in stdout, got: {stdout}"
    );
    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn bench_subcommand_handles_microsecond_workload() {
    // Microsecond-class bench - exercises the per-op timing on a
    // fn that does observable arithmetic work each call.
    let fixture = write_fixture(
        "benchharness_micro",
        "fn add_two(a: i64, b: i64) -> i64 { a + b }
#[bench]
fn bench_add_two() { let _ = add_two(1i64, 2i64) }
fn main() { }
",
    );
    let out = Command::new(gos_bin())
        .args(["bench"])
        .arg(&fixture)
        .output()
        .expect("spawn bench");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout
        .lines()
        .find(|l| l.contains("bench_add_two"))
        .unwrap_or_else(|| panic!("missing bench line in stdout: {stdout}"));
    assert!(line.contains("ns/op"));
    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn perf_gate_benchmarks_keep_work_observable() {
    let source = std::fs::read_to_string(workspace_root().join("benchmarks/perf/core.gos"))
        .expect("read perf benchmark fixture");
    for name in [
        "bench_arithmetic_loop_observed",
        "bench_vec_growth_scan_observed",
        "bench_struct_fields_observed",
        "bench_string_format_observed",
    ] {
        assert!(
            source.contains(&format!("fn {name}() -> i64")),
            "{name} must return the computed value so the perf gate measures real work",
        );
    }
    assert!(
        !source.contains("let _ = arithmetic_work(")
            && !source.contains("let _ = vec_work(")
            && !source.contains("let _ = struct_work(")
            && !source.contains("let _ = string_work("),
        "perf benchmark workloads must not be discarded with `let _ = ...`",
    );
}

#[test]
fn crypto_x509_crl_benchmark_uses_the_public_verifier_and_checks_success() {
    let fixture = workspace_root().join("benchmarks/perf/crypto_x509_crl.gos");
    let source = std::fs::read_to_string(&fixture).expect("read crypto X.509 benchmark fixture");
    assert!(
        source.contains("fn bench_crypto_x509_crl_verify_observed() -> i64")
            && source.contains("crypto::x509::verify_server_certificate_with_crls")
            && source.contains("Ok(_) => 1"),
        "the crypto benchmark must call the public verifier and retain its checked result"
    );
    let out = Command::new(gos_bin())
        .args(["bench"])
        .arg(&fixture)
        .output()
        .expect("spawn crypto X.509 benchmark");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("bench_crypto_x509_crl_verify_observed") && stdout.contains("ns/op"),
        "expected checked crypto benchmark output, got: {stdout}"
    );
}

#[test]
fn http_diagnostics_transport_benchmark_uses_loopback_fixture() {
    let fixture = workspace_root().join("benchmarks/perf/http_diagnostics_transport.gos");
    let out = Command::new(gos_bin())
        .args(["bench"])
        .arg(&fixture)
        .output()
        .expect("spawn HTTP diagnostics benchmark");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("bench_http_diagnostics_transport_observed")
            && stdout.contains("ns/op")
            && stdout.contains("allocs"),
        "expected checked-in HTTP diagnostics benchmark output, got: {stdout}"
    );
}

#[test]
fn bench_subcommand_reports_no_benches_when_none_present() {
    let fixture = write_fixture("benchharness_empty", "fn main() { }\n");
    let out = Command::new(gos_bin())
        .args(["bench"])
        .arg(&fixture)
        .output()
        .expect("spawn bench");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("no #[bench] functions"),
        "expected the empty-discovery message, got: {stdout}"
    );
    let _ = std::fs::remove_file(&fixture);
}

/// Stream A.3 - examples quality gate.
///
/// Every `.gos` file directly under `examples/` must parse cleanly
/// through `gos parse`. The runnable subset (`hello_world`,
/// `line_count`, `web_server`) is also executed by
/// `run_executes_every_example_in_examples_dir`; this gate covers the
/// rest, so a regression that breaks the shape of any example fails CI.
#[test]
fn every_top_level_example_parses() {
    let dir = examples_dir();
    let entries: Vec<_> = std::fs::read_dir(&dir)
        .expect("read examples dir")
        .filter_map(Result::ok)
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|s| s.to_str())
                .is_some_and(|ext| ext == "gos")
        })
        .collect();
    assert!(
        !entries.is_empty(),
        "examples/ must contain at least one .gos"
    );
    for entry in entries {
        let path = entry.path();
        let out = Command::new(gos_bin())
            .arg("parse")
            .arg(&path)
            .output()
            .expect("spawn parse");
        assert!(
            out.status.success(),
            "{} failed to parse: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// Stream C.3 - the `gos lint` subcommand runs against a single
/// file and reports at least one warning for code that trips a
/// day-one lint.
#[test]
fn lint_subcommand_reports_unused_variable() {
    let fixture = write_fixture("lintunused", "fn main() { let x = 1i64 }\n");
    let out = Command::new(gos_bin())
        .arg("lint")
        .arg(&fixture)
        .output()
        .expect("spawn lint");
    assert!(out.status.success(), "lint should succeed with warnings");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("GL0001"), "missing lint code: {stderr}");
    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn lint_warns_on_parenthesized_closure_parameter() {
    let fixture = write_fixture("lintclosureparen", "fn main() { let f = |(t)| t }\n");
    let out = Command::new(gos_bin())
        .arg("lint")
        .arg(&fixture)
        .output()
        .expect("spawn lint");
    assert!(out.status.success(), "lint should succeed with warnings");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("GL0028"), "missing lint code: {stderr}");
    assert!(
        stderr.contains("unnecessary parentheses around closure parameter"),
        "missing closure parameter warning: {stderr}"
    );
    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn doc_html_emits_search_bar_intra_links_and_per_item_anchors() {
    let source = "\
// Greets a person.
//
// See also [farewell].
fn greet(name: String) -> String { \"hi, \" + name }

// Parting words for [greet].
fn farewell(name: String) -> String { \"bye, \" + name }
";
    let fixture = write_fixture("dochtml", source);
    let out_path = fixture.with_extension("html");
    let out = Command::new(gos_bin())
        .arg("doc")
        .arg("--html")
        .arg(&out_path)
        .arg(&fixture)
        .output()
        .expect("spawn doc --html");
    assert!(out.status.success(), "doc should succeed: {out:?}");
    let html = std::fs::read_to_string(&out_path).expect("read rendered html");
    assert!(html.contains("id=\"q\""), "search input missing: {html}");
    assert!(
        html.contains("id=\"item-fn-greet\""),
        "per-item anchor missing: {html}"
    );
    assert!(
        html.contains("href=\"#item-fn-farewell\""),
        "intra-doc link to `farewell` missing: {html}"
    );
    assert!(
        html.contains("href=\"#item-fn-greet\""),
        "intra-doc link to `greet` missing: {html}"
    );
    let _ = std::fs::remove_file(&fixture);
    let _ = std::fs::remove_file(&out_path);
}

/// Roots for a `gos clean` case: a cache directory the sweep may remove
/// and a project directory the child runs in. The project directory is
/// kept outside the cache root because a process cannot remove its own
/// working directory on Windows.
fn clean_case_roots(label: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "gos-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let cache = root.join("cache");
    let project = root.join("project");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    (cache, project)
}

#[test]
fn clean_subcommand_removes_frontend_cache_directory() {
    let (cache, project) = clean_case_roots("clean-itest");
    std::fs::write(cache.join("abc123.ok"), b"").unwrap();
    // Point every cache class inside `cache`: `gos clean` sweeps the shared
    // per-user cache too, which other tests in this run are writing to. The
    // project-local `.gos-cache` roots come from the working directory, so
    // the child runs in a project directory of its own.
    let out = Command::new(gos_bin())
        .arg("clean")
        .current_dir(&project)
        .env("GOSSAMER_CACHE_DIR", &cache)
        .env("GOSSAMER_CACHE", &cache)
        .env("XDG_CACHE_HOME", &cache)
        .env("HOME", &cache)
        .output()
        .expect("spawn clean");
    assert!(out.status.success(), "clean should succeed: {out:?}");
    assert!(
        !cache.exists(),
        "cache dir still exists after clean: {}",
        cache.display()
    );
    let _ = std::fs::remove_dir_all(project.parent().unwrap());
}

#[test]
fn clean_dry_run_reports_sizes_without_touching_the_cache() {
    let (cache, project) = clean_case_roots("clean-dryrun");
    let tmp = cache;
    std::fs::write(tmp.join("abc123.ok"), b"hello").unwrap();
    let out = Command::new(gos_bin())
        .arg("clean")
        .arg("--dry-run")
        .current_dir(&project)
        .env("GOSSAMER_CACHE_DIR", &tmp)
        .env("GOSSAMER_CACHE", &tmp)
        .env("XDG_CACHE_HOME", &tmp)
        .env("HOME", &tmp)
        .output()
        .expect("spawn clean --dry-run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "dry-run should succeed: {out:?}");
    assert!(
        tmp.exists(),
        "cache dir should NOT be removed during dry run"
    );
    assert!(
        stdout.contains("would remove frontend cache"),
        "expected would-remove line in {stdout}"
    );
    let _ = std::fs::remove_dir_all(project.parent().unwrap());
}

#[test]
fn test_subcommand_runs_doc_tests_and_reports_failures() {
    let source = "\
// Doubles `n`.\n\
//\n\
// ```\n\
// let x = 2i64\n\
// if x * 2i64 != 4i64 { panic(\"bad\") }\n\
// ```\n\
fn double(n: i64) -> i64 { n * 2i64 }\n\
\n\
// Intentionally broken doc-test.\n\
//\n\
// ```\n\
// panic(\"boom\")\n\
// ```\n\
fn broken() {}\n";
    let fixture = write_fixture("doctest", source);
    let out = Command::new(gos_bin())
        .arg("test")
        .arg(&fixture)
        .output()
        .expect("spawn test");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("PASS doc-test"),
        "expected PASS doc-test in stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("FAIL doc-test"),
        "expected FAIL doc-test in stdout:\n{stdout}"
    );
    assert!(
        !out.status.success(),
        "broken doc-test should cause non-zero exit"
    );
    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn doc_tests_call_the_documented_item_and_report_at_the_comment() {
    let source = "\
// Adds two numbers.\n\
//\n\
// ```\n\
// assert_eq(add(2, 2), 4)\n\
// ```\n\
pub fn add(a: i64, b: i64) -> i64 { a + b }\n\
\n\
// Broken on purpose.\n\
//\n\
// ```\n\
// let x: i64 = \"no\"\n\
// ```\n\
fn main() { println(\"{}\", add(1, 2)) }\n";
    let fixture = write_fixture("doctest_scope", source);
    let out = Command::new(gos_bin())
        .arg("test")
        .arg(&fixture)
        .output()
        .expect("spawn test");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let name = fixture.file_name().unwrap().to_string_lossy().into_owned();
    let reported = |verdict: &str, suffix: &str| {
        stdout
            .lines()
            .any(|line| line.contains(verdict) && line.trim_end().ends_with(suffix))
    };
    assert!(
        reported("PASS doc-test", &format!("{name}:3")),
        "a fence calling the item it documents passes:\n{stdout}\n{stderr}"
    );
    assert!(
        reported("FAIL doc-test", &format!("{name}:10 (compile)")),
        "a fence that does not type-check fails:\n{stdout}"
    );
    assert!(
        stderr.contains(&format!("{name}:11:17")),
        "the diagnostic points at the comment line it was read from:\n{stderr}"
    );
    assert!(stdout.contains("1 assertion(s)"), "the fence's assertion counts:\n{stdout}");
    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn test_reports_module_files_by_their_own_names() {
    let mut dir = env::temp_dir();
    dir.push(format!("gos-test-modules-{}", std::process::id()));
    let src = dir.join("src");
    std::fs::create_dir_all(&src).expect("mkdir project");
    std::fs::write(
        dir.join("project.toml"),
        "[project]\nid = \"example.com/mods\"\nversion = \"0.1.0\"\n",
    )
    .expect("write manifest");
    std::fs::write(src.join("main.gos"), "use util::add\n\nfn main() { println(\"{}\", add(1, 2)) }\n")
        .expect("write main");
    std::fs::write(
        src.join("util.gos"),
        "// Adds.\n//\n// ```\n// assert_eq(add(1, 1), 2)\n// ```\n\
pub fn add(a: i64, b: i64) -> i64 { a + b }\n\n\
#[cfg(test)]\nmod tests {\n    #[test]\n    fn wrong() { assert_eq(super::add(1, 1), 3) }\n}\n",
    )
    .expect("write util");
    let out = Command::new(gos_bin())
        .arg("test")
        .current_dir(&dir)
        .output()
        .expect("spawn test");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("src/util.gos:11:"),
        "the failing test's frame names the module file:\n{stdout}"
    );
    assert!(!stdout.contains("main.gos:1"), "no frame is attributed to the entry:\n{stdout}");
    assert!(
        !stdout.contains("panic: error["),
        "the failure line carries one `panic:`:\n{stdout}"
    );
    assert_eq!(
        stdout.matches("  at ").count(),
        1,
        "the failing test is one frame:\n{stdout}"
    );
    assert!(
        stdout.contains("PASS doc-test src/util.gos:3"),
        "a module file's doc tests run:\n{stdout}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn lint_fix_applies_auto_suggestions_and_writes_back() {
    // The binding is only read as a value, so nothing here needs the `mut`.
    let source = "fn main() { let mut x = 1i64\nprintln(\"{}\", x + 1) }\n";
    let fixture = write_fixture("lintfix", source);
    let out = Command::new(gos_bin())
        .arg("lint")
        .arg("--fix")
        .arg(&fixture)
        .output()
        .expect("spawn lint --fix");
    assert!(out.status.success(), "--fix should succeed: {out:?}");
    let rewritten = std::fs::read_to_string(&fixture).expect("read rewritten file");
    assert!(
        !rewritten.contains("mut x"),
        "mut keyword should be removed: {rewritten}"
    );
    assert!(
        rewritten.contains("let x = 1i64"),
        "binding should remain: {rewritten}"
    );
    let _ = std::fs::remove_file(&fixture);
}

/// Stream C.3 - `--deny-warnings` upgrades every lint hit to an
/// error and makes the command fail.
#[test]
fn lint_deny_warnings_fails_on_lint_hit() {
    let fixture = write_fixture("lintdeny", "fn main() { let x = 1i64 }\n");
    let out = Command::new(gos_bin())
        .arg("lint")
        .arg("--deny-warnings")
        .arg(&fixture)
        .output()
        .expect("spawn lint --deny-warnings");
    assert!(!out.status.success(), "expected failure, got ok");
    let _ = std::fs::remove_file(&fixture);
}

/// Stream C.3 - `--explain <lint>` prints the long-form description.
#[test]
fn lint_explain_prints_description() {
    let out = Command::new(gos_bin())
        .arg("lint")
        .arg("--explain")
        .arg("unused_variable")
        .arg(examples_dir().join("hello_world.gos"))
        .output()
        .expect("spawn lint --explain");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("unused_variable"));
    assert!(stdout.contains("Prefix the name with `_`"));
}

/// Stream C.4 - walking the `examples/` tree produces at most a
/// warning-level output and exits zero.
#[test]
fn lint_walks_examples_directory_without_failing() {
    let out = Command::new(gos_bin())
        .arg("lint")
        .arg(examples_dir())
        .output()
        .expect("spawn lint examples/");
    assert!(
        out.status.success(),
        "gos lint examples/ failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Stream H.3 - `gos fmt` must be idempotent: formatting an
/// already-formatted file must produce zero diffs on a second pass.
#[test]
fn fmt_is_idempotent_on_the_full_examples_tree() {
    let dir = examples_dir();
    let entries: Vec<_> = std::fs::read_dir(&dir)
        .expect("read examples dir")
        .filter_map(Result::ok)
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|s| s.to_str())
                .is_some_and(|ext| ext == "gos")
        })
        .collect();
    for entry in entries {
        let source = std::fs::read_to_string(entry.path()).unwrap();
        let temp = write_fixture("fmt_idem", &source);
        // First pass: produce canonical form.
        let out = Command::new(gos_bin())
            .arg("fmt")
            .arg(&temp)
            .output()
            .expect("spawn fmt");
        assert!(
            out.status.success(),
            "fmt pass 1 failed on {}",
            entry.path().display()
        );
        let canonical = std::fs::read_to_string(&temp).unwrap();
        // Second pass must report no change and leave the file alone.
        let out = Command::new(gos_bin())
            .args(["fmt", "--check"])
            .arg(&temp)
            .output()
            .expect("spawn fmt --check");
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "fmt --check failed on already-formatted {} - stderr: {stderr} stdout: {stdout}",
            entry.path().display()
        );
        let rechecked = std::fs::read_to_string(&temp).unwrap();
        assert_eq!(
            canonical,
            rechecked,
            "fmt is not idempotent on {}",
            entry.path().display()
        );
        let _ = std::fs::remove_file(&temp);
    }
}

/// Stream H.6 - `gos explain <code>` prints the long-form
/// explanation for a diagnostic code.
#[test]
fn explain_prints_description_for_known_code() {
    let out = Command::new(gos_bin())
        .args(["explain", "GP0001"])
        .output()
        .expect("spawn explain");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("GP0001"));
    assert!(text.contains("parser"));
}

/// Every GT rejection code the checker gained this cycle has an
/// `explain` entry - a refusal pointing at an unexplained code is
/// a docs gap.
#[test]
fn explain_covers_checker_rejection_codes() {
    for (code, _) in gossamer_diagnostics::REGISTRY {
        let out = Command::new(gos_bin())
            .args(["explain", code])
            .output()
            .expect("spawn explain");
        assert!(out.status.success(), "explain {code} failed");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains(code), "explain {code} output: {text}");
    }
}

/// Stream H.6 - unknown codes produce a clear error.
#[test]
fn explain_rejects_unknown_code() {
    let out = Command::new(gos_bin())
        .args(["explain", "G99999"])
        .output()
        .expect("spawn explain");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("G99999"));
}

/// Stream H.7 - panics surface a call-stack snapshot to stderr.
#[test]
fn panic_error_includes_call_stack() {
    let fixture = write_fixture(
        "panictrace",
        "fn inner() { panic(\"boom\") }\nfn main() { inner() }\n",
    );
    let out = Command::new(gos_bin())
        .arg("run")
        .arg(&fixture)
        .output()
        .expect("spawn run");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("call stack"));
    assert!(stderr.contains("at main"));
    assert!(stderr.contains(".gos:2:13)"), "stderr: {stderr}");
    assert!(stderr.contains("at inner"));
    assert!(stderr.contains(".gos:1:14)"), "stderr: {stderr}");
    let _ = std::fs::remove_file(&fixture);
}

/// Stream A.3 - every terminating runnable example must execute
/// under the tree-walker without a runtime error. `web_server.gos`
/// is a real server; it is covered by
/// `web_server_example_binds_and_serves_real_requests`.
#[test]
fn every_terminating_example_executes_cleanly() {
    let examples: [&str; 1] = ["hello_world.gos"];
    for name in examples {
        let path = examples_dir().join(name);
        let out = Command::new(gos_bin())
            .arg("run")
            .arg(&path)
            .output()
            .expect("spawn run");
        assert!(
            out.status.success(),
            "{name} failed at runtime: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn build_rejects_unknown_target() {
    let fixture = write_fixture("buildbad", "fn main() { }\n");
    let out = Command::new(gos_bin())
        .args(["build", "--target", "not-a-triple"])
        .arg(&fixture)
        .output()
        .expect("spawn build");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown target"));
    let _ = std::fs::remove_file(&fixture);
}
