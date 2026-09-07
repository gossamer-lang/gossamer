//! `gos test [PATH] [--run RX] [--parallel N] [--format junit]
//! [--junit-out FILE] [--race] [--coverage FILE]` discovers and
//! runs every `#[test]`-annotated function under `PATH`, plus every
//! fenced doc-test it can extract from `//` comments.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};

use crate::cmd::attr_walk::item_has_attr;
use crate::loaders::{load_and_check, load_and_check_with_sf};
use crate::paths::{collect_lint_targets, default_test_root, read_entry_source, read_source};

/// ANSI styling shared by the test-runner output. Disabled when
/// stdout isn't a TTY (CI captures, pipes), or when the user
/// explicitly opts out via `NO_COLOR=1`. See <https://no-color.org>.
#[derive(Clone)]
pub(crate) struct TestStyle {
    enabled: bool,
}

impl TestStyle {
    fn detect() -> Self {
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        let tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
        Self {
            enabled: tty && !no_color,
        }
    }
    fn pass(&self) -> &'static str {
        if self.enabled {
            "\x1b[32mPASS\x1b[0m"
        } else {
            "PASS"
        }
    }
    fn fail(&self) -> &'static str {
        if self.enabled {
            "\x1b[31mFAIL\x1b[0m"
        } else {
            "FAIL"
        }
    }
    fn dim<'a>(&self, s: &'a str) -> std::borrow::Cow<'a, str> {
        if self.enabled {
            format!("\x1b[2m{s}\x1b[0m").into()
        } else {
            s.into()
        }
    }
    fn bold<'a>(&self, s: &'a str) -> std::borrow::Cow<'a, str> {
        if self.enabled {
            format!("\x1b[1m{s}\x1b[0m").into()
        } else {
            s.into()
        }
    }
    fn green<'a>(&self, s: &'a str) -> std::borrow::Cow<'a, str> {
        if self.enabled {
            format!("\x1b[32m{s}\x1b[0m").into()
        } else {
            s.into()
        }
    }
    fn red<'a>(&self, s: &'a str) -> std::borrow::Cow<'a, str> {
        if self.enabled {
            format!("\x1b[31m{s}\x1b[0m").into()
        } else {
            s.into()
        }
    }
    fn cyan<'a>(&self, s: &'a str) -> std::borrow::Cow<'a, str> {
        if self.enabled {
            format!("\x1b[36m{s}\x1b[0m").into()
        } else {
            s.into()
        }
    }
}

fn assertion_elapsed_summary(assertions: u32, elapsed_ms: u128) -> String {
    format!(
        "({assertions} {asserts}, {elapsed_ms}ms)",
        asserts = if assertions == 1 {
            "assertion"
        } else {
            "assertions"
        },
    )
}

/// Reported for a `#[test]` body that records no assertion.
const NO_ASSERTIONS_REASON: &str = "made no assertions";

/// Printed once per run, after the summary, when any test failed for
/// [`NO_ASSERTIONS_REASON`]. The guidance is the same for every such test, so
/// repeating it per test buries the failures it is meant to explain.
const NO_ASSERTIONS_HINT: &str = "note: a test decides its result with assert(cond), assert_eq(a, b), or testing::check(cond, msg) - a body that only prints cannot fail";

fn is_worker_harness_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    (trimmed.starts_with("===") && trimmed.ends_with("==="))
        || trimmed.starts_with("PASS ")
        || trimmed.starts_with("FAIL ")
        || trimmed.starts_with("test: ")
        || (trimmed.starts_with("error: ") && trimmed.ends_with("test failure(s)"))
        || trimmed.contains(NO_ASSERTIONS_HINT)
}

/// The reason carried by a worker's own `FAIL <name> (<n>ms): <reason>` line.
/// A worker prints a whole harness run, so forwarding its output verbatim would
/// show the child's tallies beside the parent's as if they were two results.
fn worker_failure_reason(captured: &str) -> Option<String> {
    captured.lines().find_map(|line| {
        let rest = line.trim_start().strip_prefix("FAIL ")?;
        let colon = rest.find(": ")?;
        Some(rest[colon + 2..].trim().to_string())
    })
}

/// Whatever the test itself wrote, with the worker's harness lines removed.
fn worker_user_output(captured: &str) -> String {
    captured
        .lines()
        .filter(|line| !is_worker_harness_line(line))
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn print_worker_user_output(captured: &str) {
    for line in captured.lines() {
        if !is_worker_harness_line(line) {
            println!("{line}");
        }
    }
}

/// Options threaded into [`run_with_opts`].
#[allow(
    clippy::struct_excessive_bools,
    reason = "CLI switches preserve independent user-selected test runner controls"
)]
pub(crate) struct TestOpts {
    pub path: Option<PathBuf>,
    pub run_filter: Option<String>,
    pub list: bool,
    pub exact: Option<String>,
    pub fail_fast: bool,
    pub include_ignored: bool,
    pub ignored_only: bool,
    pub shuffle: bool,
    pub seed: Option<u64>,
    pub timeout: Option<std::time::Duration>,
    pub worker: bool,
    pub parallel: usize,
    pub format: String,
    pub junit_out: Option<PathBuf>,
    /// Enable the runtime data-race detector while running tests.
    pub race: bool,
    /// Optional lcov-format coverage output path.
    pub coverage: Option<PathBuf>,
    /// When true, run the cross-tier parity walk (VM + compiled)
    /// instead of the per-`#[test]` discovery flow. The walk
    /// targets every `.gos` file under `path` (defaults to
    /// `examples/` + `feature-testing-examples/`) and writes its
    /// per-tier outcome into a JSON sidecar driven by `report`.
    pub tier_parity: bool,
    /// Run the fuzz loop over `#[fuzz]` functions instead of `#[test]`
    /// discovery.
    pub fuzz: bool,
    /// How long to fuzz each target. Without it the loop runs a fixed
    /// number of inputs, so a run is reproducible.
    pub fuzz_time: Option<std::time::Duration>,
    /// Sidecar emission selector. `Some("status")` writes
    /// `target/debug/.feature-status.json` consumed by
    /// `gos feature-status`. Other values are reserved for future
    /// report shapes.
    pub report: Option<String>,
}

/// Runs every committed fuzz corpus entry, returning how many failed.
///
/// Reported alongside the `#[test]` results rather than as a separate
/// command, so a committed crash cannot pass unnoticed.
fn run_fuzz_corpus_regressions(path: Option<&Path>) -> usize {
    let Ok(root) = (match path {
        Some(p) => Ok(p.to_path_buf()),
        None => default_test_root(),
    }) else {
        return 0;
    };
    let files = if root.is_file() {
        vec![root]
    } else {
        match collect_lint_targets(&root) {
            Ok(files) => files,
            Err(_) => return 0,
        }
    };
    let Ok(targets) = crate::cmd::fuzz::discover(&files) else {
        return 0;
    };
    if targets.is_empty() {
        return 0;
    }
    match crate::cmd::fuzz::run_corpus_as_tests(&targets) {
        Ok((passed, failed)) => {
            println!(
                "fuzz corpus: {passed} passed, {failed} failed across {} target(s)",
                targets.len()
            );
            failed
        }
        Err(_) => 0,
    }
}

/// Discovers `#[fuzz]` targets under the test root and fuzzes them.
fn run_fuzz(opts: &TestOpts) -> Result<()> {
    let root = match opts.path.as_ref() {
        Some(p) => p.clone(),
        None => default_test_root()?,
    };
    let files = if root.is_file() {
        vec![root]
    } else {
        collect_lint_targets(&root)?
    };
    let targets = crate::cmd::fuzz::discover(&files)?;
    let seed = opts.seed.unwrap_or(0);
    crate::cmd::fuzz::run(&targets, opts.fuzz_time, seed)
}

/// Fails when any source under `path` disagrees with `gos fmt`.
fn enforce_canonical_format(path: Option<&Path>) -> Result<()> {
    let root = match path {
        Some(p) => p.to_path_buf(),
        None => default_test_root()?,
    };
    let files = if root.is_file() {
        vec![root]
    } else {
        collect_lint_targets(&root)?
    };
    let mut drift: Vec<String> = Vec::new();
    for file in &files {
        let Ok(source) = read_source(file) else {
            continue;
        };
        let mut map = gossamer_lex::SourceMap::new();
        let file_id = map.add_file(file.to_string_lossy().into_owned(), source.clone());
        let Ok(canonical) = gossamer_parse::format_source(&source, file_id) else {
            // Unparseable input is the type checker's report to make, not
            // the formatter's.
            continue;
        };
        if canonical != source {
            drift.push(file.display().to_string());
        }
    }
    if drift.is_empty() {
        return Ok(());
    }
    Err(anyhow!(
        "project.enforce-format is set and {} file(s) are not canonically formatted; \
         run `gos fmt`: {}",
        drift.len(),
        drift.join(", "),
    ))
}

pub(crate) fn parse_timeout(input: &str) -> Result<std::time::Duration> {
    let input = input.trim();
    let (number, multiplier) = if let Some(value) = input.strip_suffix("ms") {
        (value, 1u64)
    } else if let Some(value) = input.strip_suffix('s') {
        (value, 1_000)
    } else if let Some(value) = input.strip_suffix('m') {
        (value, 60_000)
    } else {
        return Err(anyhow!(
            "invalid duration `{input}`: expected ms, s, or m suffix"
        ));
    };
    let value: u64 = number
        .parse()
        .map_err(|_| anyhow!("invalid duration `{input}`"))?;
    let millis = value
        .checked_mul(multiplier)
        .ok_or_else(|| anyhow!("duration `{input}` is too large"))?;
    if millis == 0 {
        return Err(anyhow!("timeout must be greater than zero"));
    }
    Ok(std::time::Duration::from_millis(millis))
}

/// One test outcome, structured so `JUnit` XML and the human renderer
/// share the same data.
#[derive(Debug, Clone)]
struct TestRecord {
    file: String,
    name: String,
    passed: bool,
    elapsed_ms: u128,
    failure_message: Option<String>,
    assertions: u32,
    timed_out: bool,
    status: TestStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TestStatus {
    Passed,
    Failed,
    Panicked,
    TimedOut,
    Ignored,
    Skipped,
}

#[derive(Debug, Clone)]
struct TestSpec {
    /// Byte offset of the declaring item in the assembled unit, so a test
    /// can be attributed to the file its bytes were written in.
    start: u32,
    function: String,
    name: String,
    args: Vec<gossamer_interp::Value>,
    ignored: bool,
    skipped: bool,
    timeout: Option<std::time::Duration>,
}

/// Aggregate doc-test outcome for a single source file.
struct DocTestFileSummary {
    passes: u32,
    failures: u32,
}

/// One fenced code block extracted from a `//` doc comment.
struct DocTest {
    /// Human-readable label: `<file>:<open-fence-line>`.
    name: String,
    /// Body of the fence, with `// ` prefixes stripped.
    code: String,
}

#[allow(
    clippy::cognitive_complexity,
    clippy::too_many_lines,
    reason = "test runner orchestrates discovery, parallel exec, junit output, coverage end-to-end"
)]
pub(crate) fn run_with_opts(opts: TestOpts) -> Result<()> {
    if opts.tier_parity {
        return tier_parity::run(&opts);
    }
    if opts.fuzz {
        return run_fuzz(&opts);
    }
    // A project that opted into canonical formatting gets it checked as
    // part of passing, rather than as a step someone has to remember.
    // Checked before the suite runs so the report is not buried under it.
    if crate::paths::project_enforces_format() {
        enforce_canonical_format(opts.path.as_deref())?;
    }
    // Corpus entries run as ordinary tests: once a fuzz crash is
    // committed, plain `gos test` fails until it is fixed. This is what
    // makes a fuzz finding a gate rather than a report.
    let fuzz_regressions = run_fuzz_corpus_regressions(opts.path.as_deref());
    gossamer_resolve::set_test_cfg(true);
    // Run tests on pure bytecode. The per-test assertion tally and the
    // coverage counters are interp-side mechanisms the whole-program
    // JIT bypasses (a JIT-promoted `#[test]` would record neither), so
    // a prior test warming up the JIT must not silently drop a later
    // test's assertions. Determinism over throughput here.
    gossamer_interp::set_jit_disabled();
    if opts.race {
        gossamer_runtime::race::enable();
        gossamer_codegen_llvm::set_race_instrumentation(true);
    }
    if opts.coverage.is_some() {
        gossamer_runtime::coverage::set_enabled(true);
        gossamer_runtime::coverage::reset();
    }
    let resolved = match opts.path.as_ref() {
        Some(p) => p.clone(),
        None => default_test_root()?,
    };
    let style = TestStyle::detect();
    let files = test_units(&resolved)?;
    if files.is_empty() {
        return Err(anyhow!(
            "no `.gos` sources found under {}",
            resolved.display()
        ));
    }
    let want_junit = opts.format == "junit";
    let filter = if let Some(pat) = opts.run_filter.as_deref() {
        Some(regex::Regex::new(pat).map_err(|e| anyhow!("invalid --run regex `{pat}`: {e}"))?)
    } else {
        None
    };

    let mut discovered: Vec<(PathBuf, TestSpec)> = Vec::new();
    let mut records: Vec<TestRecord> = Vec::new();
    let mut load_errors: Vec<String> = Vec::new();
    for file in &files {
        let tests = match discover_tests(file) {
            Ok(names) => names,
            Err(err) => {
                // The diagnostic itself was already streamed to
                // stderr by `load_and_check_with_sf`; surface the
                // accompanying anyhow trailer ("N … error(s);
                // refusing to execute") so the user sees the
                // refusal explicitly.
                eprintln!("error: {err}");
                load_errors.push(format!("{}: {err}", file.display()));
                continue;
            }
        };
        if opts.list {
            for test in tests {
                if filter
                    .as_ref()
                    .is_some_and(|regex| !regex.is_match(&test.name))
                    || opts.exact.as_ref().is_some_and(|exact| exact != &test.name)
                    || (opts.ignored_only && !test.ignored)
                {
                    continue;
                }
                let suffix = if test.ignored {
                    " (ignored)"
                } else if test.skipped {
                    " (skipped)"
                } else {
                    ""
                };
                println!("{}::{}{suffix}", file.display(), test.name);
            }
            continue;
        }
        if !tests.is_empty() {
            if let Err(err) = validate_test_file(file) {
                eprintln!("error: {err}");
                load_errors.push(format!("{}: {err}", file.display()));
                continue;
            }
        }
        for test in tests {
            if let Some(re) = filter.as_ref() {
                if !re.is_match(&test.name) {
                    continue;
                }
            }
            if opts.exact.as_ref().is_some_and(|exact| exact != &test.name) {
                continue;
            }
            let run_ignored = opts.include_ignored || opts.ignored_only;
            if test.skipped
                || (test.ignored && !run_ignored)
                || (opts.ignored_only && !test.ignored)
            {
                if !opts.ignored_only || test.ignored {
                    let status = if test.skipped {
                        TestStatus::Skipped
                    } else {
                        TestStatus::Ignored
                    };
                    if !want_junit {
                        let label = if status == TestStatus::Skipped {
                            "SKIPPED"
                        } else {
                            "IGNORED"
                        };
                        println!("  {} {}::{}", style.dim(label), file.display(), test.name);
                    }
                    records.push(TestRecord {
                        file: file.to_string_lossy().into_owned(),
                        name: test.name,
                        passed: true,
                        elapsed_ms: 0,
                        failure_message: None,
                        assertions: 0,
                        timed_out: false,
                        status,
                    });
                }
                continue;
            }
            discovered.push((file.clone(), test));
        }
    }

    if opts.list {
        return Ok(());
    }

    if opts.shuffle {
        let seed = opts.seed.unwrap_or_else(|| {
            use std::time::{SystemTime, UNIX_EPOCH};
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos() as u64)
        });
        println!("test: shuffle seed {seed}");
        deterministic_shuffle(&mut discovered, seed);
    }

    let mut total_doc_passes = 0u32;
    let mut total_doc_failures = 0u32;

    let by_file: std::collections::BTreeMap<PathBuf, Vec<TestSpec>> = {
        let mut map: std::collections::BTreeMap<PathBuf, Vec<TestSpec>> =
            std::collections::BTreeMap::new();
        for (f, n) in discovered {
            map.entry(f).or_default().push(n);
        }
        map
    };

    let parallel = if opts.fail_fast {
        1
    } else {
        opts.parallel.max(1)
    };
    let by_file_vec: Vec<(PathBuf, Vec<TestSpec>)> = by_file.into_iter().collect();
    // Process isolation is the normal mode so cwd, environment, output, and
    // leaked goroutines cannot contaminate the next test. Coverage and race
    // counters currently remain in-process because their collectors are
    // process-local; timeout metadata still forces isolation for either mode.
    let has_test_timeout = by_file_vec
        .iter()
        .any(|(_, tests)| tests.iter().any(|test| test.timeout.is_some()));
    let needs_isolation = !opts.worker
        && ((!opts.race && opts.coverage.is_none()) || opts.timeout.is_some() || has_test_timeout);
    let collected: Vec<(PathBuf, Vec<TestRecord>)> = if needs_isolation {
        run_tests_isolated(
            &by_file_vec,
            opts.timeout,
            opts.fail_fast,
            &style,
            want_junit,
        )?
    } else if parallel > 1 && by_file_vec.len() > 1 {
        run_files_parallel(&by_file_vec, parallel, &style, want_junit)
    } else {
        let mut output = Vec::new();
        for (file, names) in &by_file_vec {
            let recs = run_tests_filtered(file, names, &style, want_junit);
            let failed = recs.iter().any(|record| !record.passed);
            output.push((file.clone(), recs));
            if opts.fail_fast && failed {
                break;
            }
        }
        output
    };
    for (_, recs) in collected {
        records.extend(recs);
    }
    if !opts.worker {
        for file in &files {
            let doc_summary = run_doc_tests_in_file(file, &style);
            total_doc_passes += doc_summary.passes;
            total_doc_failures += doc_summary.failures;
        }
    }

    let total_passes = u32::try_from(
        records
            .iter()
            .filter(|r| r.status == TestStatus::Passed)
            .count(),
    )
    .unwrap_or(0)
        + total_doc_passes;
    let total_failures = u32::try_from(
        records
            .iter()
            .filter(|r| {
                matches!(
                    r.status,
                    TestStatus::Failed | TestStatus::Panicked | TestStatus::TimedOut
                )
            })
            .count(),
    )
    .unwrap_or(0)
        + total_doc_failures;
    let total_assertions: u32 = records.iter().map(|r| r.assertions).sum();
    let total_ignored = records
        .iter()
        .filter(|r| r.status == TestStatus::Ignored)
        .count();
    let total_skipped = records
        .iter()
        .filter(|r| r.status == TestStatus::Skipped)
        .count();
    let total_doc_tests = total_doc_passes + total_doc_failures;
    let empty_files = u32::try_from(
        files
            .iter()
            .filter(|f| !records.iter().any(|r| r.file == f.to_string_lossy()))
            .count(),
    )
    .unwrap_or(0);

    if want_junit {
        let xml = render_junit(&records);
        if let Some(out) = opts.junit_out.as_ref() {
            std::fs::write(out, &xml)
                .map_err(|e| anyhow!("write junit xml to {}: {e}", out.display()))?;
        } else {
            print!("{xml}");
        }
    } else {
        if total_passes == 0
            && total_failures == 0
            && total_doc_tests == 0
            && load_errors.is_empty()
        {
            // Help users distinguish "all tests passed" (which
            // can also be 0/0 when nothing matched a `--run`
            // filter) from "the file genuinely has nothing
            // marked `#[test]`".
            println!(
                "test: no #[test] functions found under {}",
                resolved.display()
            );
        }
        let pass_part = format!("{total_passes} passed");
        let fail_part = format!("{total_failures} failed");
        let pass_styled = if total_failures == 0 {
            style.green(&style.bold(&pass_part)).into_owned()
        } else {
            style.green(&pass_part).into_owned()
        };
        let fail_styled = if total_failures > 0 {
            style.red(&style.bold(&fail_part)).into_owned()
        } else {
            style.dim(&fail_part).into_owned()
        };
        let trailing = format!(
            "{total_assertions} assertion(s), {total_ignored} ignored, {total_skipped} skipped, {total_doc_tests} doc-test(s), across {} file(s), {empty_files} with no tests",
            files.len()
        );
        println!(
            "test: {pass_styled}, {fail_styled}, {}",
            style.dim(&trailing)
        );
        if records.iter().any(|record| {
            record
                .failure_message
                .as_deref()
                .is_some_and(|message| message.contains(NO_ASSERTIONS_REASON))
        }) {
            println!("{}", style.dim(NO_ASSERTIONS_HINT));
        }
    }
    let total_failures = total_failures + u32::try_from(fuzz_regressions).unwrap_or(u32::MAX);
    if total_failures > 0 {
        return Err(anyhow!("{total_failures} test failure(s)"));
    }
    if !load_errors.is_empty() {
        // A file the user pointed at refused to parse / resolve /
        // typecheck. Bubble up so the harness exits non-zero -
        // running tests against statically-broken source is worse
        // than reporting nothing.
        let summary = if load_errors.len() == 1 {
            "1 file failed to load".to_string()
        } else {
            format!("{} files failed to load", load_errors.len())
        };
        return Err(anyhow!("{summary}"));
    }
    if opts.race {
        let races = gossamer_runtime::race::drain_races();
        if !races.is_empty() {
            for race in &races {
                eprintln!("{race}");
            }
            return Err(anyhow!(
                "{} data race(s) detected (see --race output above)",
                races.len()
            ));
        }
    }
    if let Some(out) = opts.coverage.as_ref() {
        let lcov = render_lcov(&records, &files);
        std::fs::write(out, lcov)
            .map_err(|e| anyhow!("write coverage to {}: {e}", out.display()))?;
    }
    Ok(())
}

/// Renders an lcov report from the runtime coverage counter
/// snapshot plus the per-test-record function-level summary.
///
/// 0.8.0: real per-line + per-branch counters land via
/// `gos_rt_cov_record` calls emitted by codegen / the interp at
/// every statement boundary. The previous synthetic per-function
/// shape is preserved as the `FN:`/`FNF:`/`FNH:` fallback for
/// files that ran no instrumented code (header-only, doc-only,
/// no executable line). Format conforms to the lcov spec so
/// `genhtml`, `lcov-summary`, and CI dashboards parse it
/// directly.
fn render_lcov(records: &[TestRecord], files: &[PathBuf]) -> String {
    use std::collections::BTreeMap;

    let snapshot = gossamer_runtime::coverage::snapshot();

    let mut lines_by_file: BTreeMap<String, BTreeMap<u32, u64>> = BTreeMap::new();
    let mut branches_by_file: BTreeMap<String, BTreeMap<(u32, u32), u64>> = BTreeMap::new();
    for c in &snapshot {
        if c.branch == 0 {
            *lines_by_file
                .entry(c.file.clone())
                .or_default()
                .entry(c.line)
                .or_insert(0) += c.hits;
        } else {
            *branches_by_file
                .entry(c.file.clone())
                .or_default()
                .entry((c.line, c.branch))
                .or_insert(0) += c.hits;
        }
    }

    let mut by_file: BTreeMap<&str, (u32, u32)> = BTreeMap::new();
    for record in records {
        let entry = by_file.entry(record.file.as_str()).or_insert((0, 0));
        if record.passed {
            entry.0 += 1;
        }
        entry.1 += 1;
    }

    let mut out = String::new();
    for file in files {
        let path = file.to_string_lossy();
        let (fns_hit, fns_total) = by_file.get(path.as_ref()).copied().unwrap_or((0, 0));
        out.push_str("TN:\n");
        out.push_str(&format!("SF:{path}\n"));
        out.push_str(&format!("FNF:{fns_total}\n"));
        out.push_str(&format!("FNH:{fns_hit}\n"));
        if let Some(lines) = lines_by_file.get(path.as_ref()) {
            let mut lh = 0u32;
            for (&line, &hits) in lines {
                out.push_str(&format!("DA:{line},{hits}\n"));
                if hits > 0 {
                    lh += 1;
                }
            }
            out.push_str(&format!("LF:{}\n", lines.len()));
            out.push_str(&format!("LH:{lh}\n"));
        }
        if let Some(branches) = branches_by_file.get(path.as_ref()) {
            let mut brh = 0u32;
            for (&(line, branch), &hits) in branches {
                out.push_str(&format!("BRDA:{line},0,{branch},{hits}\n"));
                if hits > 0 {
                    brh += 1;
                }
            }
            out.push_str(&format!("BRF:{}\n", branches.len()));
            out.push_str(&format!("BRH:{brh}\n"));
        }
        out.push_str("end_of_record\n");
    }
    out
}

/// The compilation units to run tests from.
///
/// A package is one unit rooted at its entry, so a `#[test]` in a sibling
/// module runs with the whole module tree in scope - re-rooting each file
/// would turn its siblings into modules of it and leave cross-module paths
/// unresolvable. A path naming a single file, or a directory that is not a
/// package, keeps the file-per-unit reading.
fn test_units(resolved: &PathBuf) -> Result<Vec<PathBuf>> {
    if resolved.is_dir()
        && crate::paths::project_root_for_entry(&resolved.join("project.toml")).is_some()
        && let Ok(entry) = crate::paths::resolve_project_entry(resolved)
    {
        let mut units = vec![entry];
        // An integration test under `tests/` is its own program, not part of
        // the package's module tree, so it stays its own unit.
        let integration = resolved.join("tests");
        if integration.is_dir() {
            units.extend(collect_lint_targets(&integration)?);
        }
        return Ok(units);
    }
    // A directory that is not itself a package may still hold several: each
    // one is a single unit rooted at its entry, exactly as above.
    Ok(crate::paths::group_targets_by_project(
        &collect_lint_targets(resolved)?,
    ))
}

fn discover_tests(file: &Path) -> Result<Vec<TestSpec>> {
    // The assembled unit, not the file's own bytes: a package entry
    // carries its sibling modules inlined, and a `#[test]` inside one of
    // them is only visible in the assembled source.
    let unit = crate::paths::read_entry_unit(file)?;
    let mut map = gossamer_lex::SourceMap::new();
    let file_id = map.add_file(file.to_string_lossy().into_owned(), unit.source.clone());
    let (sf, _diags) = gossamer_parse::parse_source_file(&unit.source, file_id);
    let mut tests = Vec::new();
    collect_test_metadata(&sf.items, &mut tests)?;
    // A test belongs to the project whose source declares it. The unit also
    // carries every path dependency's source inlined, so without this filter
    // a dependency's tests run again for each project that depends on it.
    let foreign = foreign_regions(&unit);
    if foreign.is_empty() {
        return Ok(tests);
    }
    tests.retain(|test| {
        !foreign
            .iter()
            .any(|(lo, hi)| test.start >= *lo && test.start < *hi)
    });
    Ok(tests)
}

/// Byte ranges of the assembled unit whose bytes came from outside the
/// project being tested - a path dependency's source, inlined by the
/// bundler. Empty when the whole unit is the project's own.
fn foreign_regions(unit: &crate::paths::EntryUnit) -> Vec<(u32, u32)> {
    let Some(root) = unit
        .entry
        .parent()
        .and_then(|src| crate::paths::project_root_for_entry(&src.join("project.toml")))
        .or_else(|| {
            unit.entry
                .parent()
                .and_then(std::path::Path::parent)
                .map(std::path::Path::to_path_buf)
        })
    else {
        return Vec::new();
    };
    unit.origins
        .iter()
        .filter(|span| !span.origin.starts_with(&root))
        .map(|span| (span.start, span.end))
        .collect()
}

fn collect_test_metadata(items: &[gossamer_ast::Item], output: &mut Vec<TestSpec>) -> Result<()> {
    for item in items {
        match &item.kind {
            gossamer_ast::ItemKind::Fn(decl) if item_has_attr(item, "test") => {
                let cases: Vec<_> = item
                    .attrs
                    .outer
                    .iter()
                    .filter(|attr| {
                        attr.path
                            .segments
                            .last()
                            .is_some_and(|segment| segment.name.name == "test_case")
                    })
                    .collect();
                let timeout = match item.attrs.outer.iter().find(|attr| {
                    attr.path
                        .segments
                        .last()
                        .is_some_and(|segment| segment.name.name == "timeout")
                }) {
                    Some(attr) => Some(parse_timeout(
                        attr.tokens
                            .as_deref()
                            .ok_or_else(|| anyhow!("#[timeout] requires a duration"))?
                            .trim_matches('"'),
                    )?),
                    None => None,
                };
                let mut push_case = |args: Vec<gossamer_interp::Value>, suffix: Option<String>| {
                    output.push(TestSpec {
                        start: item.span.start,
                        function: decl.name.name.clone(),
                        name: suffix.map_or_else(
                            || decl.name.name.clone(),
                            |suffix| format!("{}[{suffix}]", decl.name.name),
                        ),
                        args,
                        ignored: item_has_attr(item, "ignore"),
                        skipped: item_has_attr(item, "skip"),
                        timeout,
                    });
                };
                if cases.is_empty() {
                    push_case(Vec::new(), None);
                } else {
                    for attr in cases {
                        let raw = attr.tokens.as_deref().unwrap_or("");
                        let args = parse_test_case_args(raw)?;
                        push_case(args, Some(raw.to_string()));
                    }
                }
            }
            gossamer_ast::ItemKind::Mod(module) => {
                if let gossamer_ast::ModBody::Inline(items) = &module.body {
                    collect_test_metadata(items, output)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn parse_test_case_args(raw: &str) -> Result<Vec<gossamer_interp::Value>> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut quote = None;
    let mut escaped = false;
    for (index, ch) in raw.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && quote.is_some() {
            escaped = true;
            continue;
        }
        if matches!(ch, '\'' | '"') {
            if quote == Some(ch) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(ch);
            }
        } else if ch == ',' && quote.is_none() {
            parts.push(raw[start..index].trim());
            start = index + 1;
        }
    }
    if quote.is_some() {
        return Err(anyhow!("unterminated string in #[test_case({raw})]"));
    }
    if !raw.trim().is_empty() {
        parts.push(raw[start..].trim());
    }
    parts.into_iter().map(parse_test_case_literal).collect()
}

fn parse_test_case_literal(raw: &str) -> Result<gossamer_interp::Value> {
    let compact_number;
    let raw = if raw.starts_with("- ") || raw.starts_with("+ ") {
        compact_number = raw.replace(' ', "");
        compact_number.as_str()
    } else {
        raw
    };
    if raw == "true" {
        return Ok(gossamer_interp::Value::Bool(true));
    }
    if raw == "false" {
        return Ok(gossamer_interp::Value::Bool(false));
    }
    if let Ok(value) = raw.parse::<i64>() {
        return Ok(gossamer_interp::Value::Int(value));
    }
    if let Ok(value) = raw.parse::<f64>() {
        return Ok(gossamer_interp::Value::Float(value));
    }
    if raw.starts_with('"') && raw.ends_with('"') && raw.len() >= 2 {
        let mut decoded = String::new();
        let mut chars = raw[1..raw.len() - 1].chars();
        while let Some(ch) = chars.next() {
            if ch != '\\' {
                decoded.push(ch);
                continue;
            }
            decoded.push(
                match chars
                    .next()
                    .ok_or_else(|| anyhow!("incomplete escape in #[test_case]"))?
                {
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    '\\' => '\\',
                    '"' => '"',
                    escape => {
                        return Err(anyhow!("unsupported escape `\\{escape}` in #[test_case]"));
                    }
                },
            );
        }
        return Ok(gossamer_interp::Value::String(decoded.into()));
    }
    Err(anyhow!(
        "#[test_case] arguments must be bool, number, or string literals; found `{raw}`"
    ))
}

fn validate_test_file(file: &Path) -> Result<()> {
    // The file carries tests: refuse to run any if the program does not
    // statically resolve and typecheck (a harness over broken code is worse
    // than useless). Validate the bundled whole-package source - the same
    // augment + comptime-fold + check the execution path uses - so cross-file
    // references stay valid and a real static error is surfaced with its
    // "refusing to execute" trailer rather than swallowed.
    let entry = read_entry_source(file)?;
    let augmented = gossamer_parse::autoderive::augment_source(&entry);
    let augmented = if augmented.contains("comptime") {
        // A comptime region that will not evaluate is a static failure, and
        // the constant it should have produced is what the tests run
        // against. Falling back to the unfolded source runs them against a
        // program the compiled tiers refuse to build.
        crate::comptime_fold::fold_comptime(augmented, &file.to_string_lossy())?
    } else {
        augmented
    };
    let mut check_map = gossamer_lex::SourceMap::new();
    let check_id = check_map.add_file(file.to_string_lossy().into_owned(), augmented.clone());
    let _ = load_and_check_with_sf(&augmented, check_id, &check_map)?;
    Ok(())
}

fn deterministic_shuffle<T>(values: &mut [T], mut state: u64) {
    if state == 0 {
        state = 0x9e37_79b9_7f4a_7c15;
    }
    for index in (1..values.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        values.swap(index, (state as usize) % (index + 1));
    }
}

fn run_tests_filtered(
    file: &Path,
    tests: &[TestSpec],
    style: &TestStyle,
    quiet: bool,
) -> Vec<TestRecord> {
    // Execute on a thread with a large native stack so a deeply
    // recursive `#[test]` does not overflow the host's default
    // main-thread stack (see `cmd::with_vm_stack`). Covers both the
    // serial and the parallel-worker call sites.
    let file = file.to_path_buf();
    let tests = tests.to_vec();
    let style = style.clone();
    crate::cmd::with_vm_stack(move || run_tests_filtered_inner(&file, &tests, &style, quiet))
}

#[allow(
    clippy::too_many_lines,
    reason = "one VM session owns test execution, tally capture, and stable reporting"
)]
fn run_tests_filtered_inner(
    file: &Path,
    tests: &[TestSpec],
    style: &TestStyle,
    quiet: bool,
) -> Vec<TestRecord> {
    // Bundle sibling `*.gos` modules into the compilation, exactly as
    // `gos` / `gos build` do, so a `#[test]` can reach a sibling
    // module (`super::helper::triple` where `src/helper.gos` is declared
    // `mod helper;`). Test-name collection stays unbundled so sibling
    // tests are not double-counted against this file.
    let Ok(source) = read_entry_source(file) else {
        return Vec::new();
    };
    let augmented = gossamer_parse::autoderive::augment_source(&source);
    // Comptime fold so a `#[test]` compiles the same constant the run /
    // build tiers do. A failure here is reported as a failing record rather
    // than skipped: `--parallel` reaches this without the static validator,
    // and a suite that answers green because its bodies never ran is the
    // one outcome a test command must never produce.
    let augmented = if augmented.contains("comptime") {
        match crate::comptime_fold::fold_comptime(augmented.clone(), &file.to_string_lossy()) {
            Ok(folded) => folded,
            Err(error) => {
                return vec![TestRecord {
                    file: file.display().to_string(),
                    name: "comptime".to_string(),
                    passed: false,
                    elapsed_ms: 0,
                    failure_message: Some(format!(
                        "compile-time evaluation failed, so no test in this file ran: {error}"
                    )),
                    assertions: 0,
                    timed_out: false,
                    status: TestStatus::Failed,
                }];
            }
        }
    } else {
        augmented
    };
    let mut map = gossamer_lex::SourceMap::new();
    let file_id = map.add_file(file.to_string_lossy().into_owned(), augmented.clone());
    let Ok((program, _sf, tcx)) = load_and_check_with_sf(&augmented, file_id, &map) else {
        return Vec::new();
    };
    let mut vm = gossamer_interp::Vm::new();
    // Publish the source map before `load` for runtime traceback locations and
    // per-statement coverage hits when `gos test --coverage` is active.
    vm.set_source_map(std::sync::Arc::new(map));
    // A test run calls each test rather than the program entry, so those are
    // the names whose bodies the promotion snapshot has to keep.
    let entries: Vec<String> = tests.iter().map(|t| t.function.clone()).collect();
    vm.set_entry_points(&entries);
    if vm.load(&program, tcx, false).is_err() {
        return Vec::new();
    }
    vm.clear_source_map();
    let mut records = Vec::new();
    if !quiet && !tests.is_empty() {
        let header = format!("=== {} ===", file.display());
        println!("{}", style.cyan(&header));
    }
    for test in tests {
        gossamer_interp::reset_test_tally();
        let started = std::time::Instant::now();
        let outcome = vm.call(&test.function, test.args.clone());
        let elapsed = started.elapsed();
        // Snapshot the VM call chain immediately: a failing test
        // preserves its frames, and the next `vm.call` clears them.
        let call_trace = if outcome.is_err() {
            crate::cmd::traceback::render_call_stack(&vm.call_stack_frames())
        } else {
            String::new()
        };
        let tally = gossamer_interp::take_test_tally();
        let panicked = outcome.as_ref().err().map(ToString::to_string);
        // A test declared `-> Result<(), E>` reports its failure by returning
        // `Err`, which reaches here as an ordinary value.
        let returned_err = outcome
            .as_ref()
            .ok()
            .and_then(gossamer_interp::err_payload_message);
        let assertion_failure = tally.failures > 0;
        // A body that records no assertion proves nothing, so it cannot certify
        // itself green. A test declared `-> Result<(), E>` is exempt: reaching
        // `Ok` past every `?` is the verdict it reports in place of an assertion.
        let asserted_nothing = tally.assertions == 0
            && panicked.is_none()
            && returned_err.is_none()
            && !assertion_failure
            && !outcome.as_ref().is_ok_and(gossamer_interp::is_ok_variant);
        let passed =
            panicked.is_none() && returned_err.is_none() && !assertion_failure && !asserted_nothing;
        let status = if passed {
            TestStatus::Passed
        } else if panicked.is_some() {
            TestStatus::Panicked
        } else {
            TestStatus::Failed
        };
        let mut failure_message: Option<String> = None;
        if !passed {
            let mut reason = String::new();
            if let Some(err) = panicked.as_deref() {
                reason.push_str(&format!("panic: {err}"));
            }
            if let Some(err) = returned_err.as_deref() {
                if !reason.is_empty() {
                    reason.push_str(" · ");
                }
                reason.push_str(&format!("returned Err: {err}"));
            }
            if assertion_failure {
                if !reason.is_empty() {
                    reason.push_str(" · ");
                }
                reason.push_str(&format!("{} assertion(s) failed", tally.failures));
                if let Some(first) = tally.first_failure.as_ref() {
                    reason.push_str(" - ");
                    reason.push_str(first);
                }
            }
            if asserted_nothing {
                if !reason.is_empty() {
                    reason.push_str(" · ");
                }
                reason.push_str(NO_ASSERTIONS_REASON);
            }
            failure_message = Some(reason);
        }
        records.push(TestRecord {
            file: file.to_string_lossy().into_owned(),
            name: test.name.clone(),
            passed,
            elapsed_ms: elapsed.as_millis(),
            failure_message: failure_message.clone(),
            assertions: tally.assertions,
            timed_out: false,
            status,
        });
        if !quiet {
            if passed {
                let assertion_summary =
                    assertion_elapsed_summary(tally.assertions, elapsed.as_millis());
                println!(
                    "  {} {} {}",
                    style.pass(),
                    test.name,
                    style.dim(&assertion_summary)
                );
            } else {
                let elapsed_str = format!("({}ms)", elapsed.as_millis());
                println!(
                    "  {} {} {}: {}",
                    style.fail(),
                    test.name,
                    style.dim(&elapsed_str),
                    style.red(&failure_message.clone().unwrap_or_default())
                );
                // The call-chain traceback is additional context - the
                // failure message above stays byte-identical to before.
                if !call_trace.is_empty() {
                    println!("{}", style.dim(&call_trace));
                }
            }
        }
    }
    records
}

type FileQueue = std::sync::Arc<parking_lot::Mutex<Vec<(PathBuf, Vec<TestSpec>)>>>;
type FileResults = std::sync::Arc<parking_lot::Mutex<Vec<(PathBuf, Vec<TestRecord>)>>>;

#[allow(
    clippy::too_many_lines,
    reason = "process lifecycle and timeout cleanup stay together for isolation safety"
)]
fn run_tests_isolated(
    by_file: &[(PathBuf, Vec<TestSpec>)],
    default_timeout: Option<std::time::Duration>,
    fail_fast: bool,
    style: &TestStyle,
    quiet: bool,
) -> Result<Vec<(PathBuf, Vec<TestRecord>)>> {
    let executable =
        std::env::current_exe().map_err(|error| anyhow!("locate test worker: {error}"))?;
    let mut output = Vec::new();
    let mut stop = false;
    for (file, tests) in by_file {
        let mut records = Vec::new();
        for test in tests {
            if stop {
                break;
            }
            let timeout = test.timeout.or(default_timeout);
            let mut command = std::process::Command::new(&executable);
            command
                .args([
                    "test",
                    &file.to_string_lossy(),
                    "--exact",
                    &test.name,
                    "--serial",
                    "--test-worker",
                ])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            if test.ignored {
                command.arg("--include-ignored");
            }
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                command.process_group(0);
            }
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x0000_0200);
            }
            let mut child = command
                .spawn()
                .map_err(|error| anyhow!("spawn isolated test `{}`: {error}", test.name))?;
            // The worker leads a process group of its own so a timeout can end
            // the whole tree it builds. That also puts it outside the terminal's
            // foreground group, so this command owns its lifetime: an interrupt
            // reaches it through the registry, and the guard drops the entry
            // however this iteration ends.
            let _child_guard = crate::child_processes::register(child.id());
            let stdout = child.stdout.take().map(|mut stream| {
                std::thread::spawn(move || {
                    let mut bytes = Vec::new();
                    let _ = stream.read_to_end(&mut bytes);
                    bytes
                })
            });
            let stderr = child.stderr.take().map(|mut stream| {
                std::thread::spawn(move || {
                    let mut bytes = Vec::new();
                    let _ = stream.read_to_end(&mut bytes);
                    bytes
                })
            });
            let started = std::time::Instant::now();
            let (status, timed_out) = if let Some(timeout) = timeout {
                loop {
                    if let Some(status) = child.try_wait().map_err(|error| {
                        anyhow!("wait for isolated test `{}`: {error}", test.name)
                    })? {
                        break (Some(status), false);
                    }
                    if started.elapsed() >= timeout {
                        let _ = gossamer_std::exec::send_group_term(i64::from(child.id()));
                        let _ = child.kill();
                        let _ = child.wait();
                        break (None, true);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            } else {
                (
                    Some(child.wait().map_err(|error| {
                        anyhow!("wait for isolated test `{}`: {error}", test.name)
                    })?),
                    false,
                )
            };
            let stdout = stdout
                .and_then(|thread| thread.join().ok())
                .unwrap_or_default();
            let stderr = stderr
                .and_then(|thread| thread.join().ok())
                .unwrap_or_default();
            let captured = format!(
                "{}{}",
                String::from_utf8_lossy(&stdout),
                String::from_utf8_lossy(&stderr)
            );
            let passed = status.is_some_and(|status| status.success()) && !timed_out;
            // The worker's own summary line is the authority on the tally;
            // a per-test line reports the same count in a different shape.
            let assertions = captured
                .lines()
                .filter(|line| line.trim_start().starts_with("test: "))
                .find_map(|line| {
                    let marker = line.find(" assertion")?;
                    line[..marker]
                        .split_whitespace()
                        .last()?
                        .parse::<u32>()
                        .ok()
                })
                .unwrap_or(0);
            let failure_message = (!passed).then(|| {
                if timed_out {
                    format!(
                        "timeout after {}ms",
                        timeout.expect("timed out only with deadline").as_millis()
                    )
                } else {
                    let output = worker_user_output(&captured);
                    match (worker_failure_reason(&captured), output.is_empty()) {
                        (Some(reason), true) => reason,
                        (Some(reason), false) => format!("{reason}\n{output}"),
                        (None, false) => output,
                        (None, true) => "isolated test failed".to_string(),
                    }
                }
            });
            let status = if timed_out {
                TestStatus::TimedOut
            } else if passed {
                TestStatus::Passed
            } else if captured.contains("panic:") {
                TestStatus::Panicked
            } else {
                TestStatus::Failed
            };
            let record = TestRecord {
                file: file.to_string_lossy().into_owned(),
                name: test.name.clone(),
                passed,
                elapsed_ms: started.elapsed().as_millis(),
                failure_message,
                assertions,
                timed_out,
                status,
            };
            if !quiet {
                if record.passed {
                    print_worker_user_output(&captured);
                    let assertion_summary =
                        assertion_elapsed_summary(record.assertions, record.elapsed_ms);
                    println!(
                        "  {} {} {}",
                        style.pass(),
                        record.name,
                        style.dim(&assertion_summary)
                    );
                } else {
                    println!(
                        "  {} {}: {}",
                        style.fail(),
                        record.name,
                        record.failure_message.as_deref().unwrap_or("failed")
                    );
                }
            }
            stop = fail_fast && !record.passed;
            records.push(record);
        }
        output.push((file.clone(), records));
        if stop {
            break;
        }
    }
    Ok(output)
}

fn run_files_parallel(
    by_file: &[(PathBuf, Vec<TestSpec>)],
    parallel: usize,
    style: &TestStyle,
    quiet: bool,
) -> Vec<(PathBuf, Vec<TestRecord>)> {
    use std::sync::Arc;

    use parking_lot::Mutex as PlMutex;
    let queue: FileQueue = Arc::new(PlMutex::new(by_file.to_vec()));
    let results: FileResults = Arc::new(PlMutex::new(Vec::new()));
    let n_workers = parallel.min(by_file.len()).max(1);
    let mut handles = Vec::with_capacity(n_workers);
    for _ in 0..n_workers {
        let queue = Arc::clone(&queue);
        let results = Arc::clone(&results);
        let style_owned: TestStyle = style.clone();
        handles.push(std::thread::spawn(move || {
            loop {
                let next = {
                    let mut q = queue.lock();
                    q.pop()
                };
                let Some((file, names)) = next else {
                    return;
                };
                let recs = run_tests_filtered(&file, &names, &style_owned, quiet);
                results.lock().push((file, recs));
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
    let mut out = Arc::try_unwrap(results)
        .expect("results arc unwrap")
        .into_inner();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn render_junit(records: &[TestRecord]) -> String {
    use std::collections::BTreeMap;
    let mut suites: BTreeMap<&str, Vec<&TestRecord>> = BTreeMap::new();
    for record in records {
        suites.entry(record.file.as_str()).or_default().push(record);
    }
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    let total_tests = records.len();
    let total_failures = records
        .iter()
        .filter(|r| matches!(r.status, TestStatus::Failed | TestStatus::Panicked))
        .count();
    let total_errors = records
        .iter()
        .filter(|r| r.status == TestStatus::TimedOut)
        .count();
    let total_skipped = records
        .iter()
        .filter(|r| matches!(r.status, TestStatus::Ignored | TestStatus::Skipped))
        .count();
    out.push_str(&format!(
        "<testsuites tests=\"{total_tests}\" failures=\"{total_failures}\" errors=\"{total_errors}\" skipped=\"{total_skipped}\">\n"
    ));
    for (suite, tests) in &suites {
        let n = tests.len();
        let failures = tests
            .iter()
            .filter(|r| matches!(r.status, TestStatus::Failed | TestStatus::Panicked))
            .count();
        let errors = tests
            .iter()
            .filter(|r| r.status == TestStatus::TimedOut)
            .count();
        let skipped = tests
            .iter()
            .filter(|r| matches!(r.status, TestStatus::Ignored | TestStatus::Skipped))
            .count();
        let total_ms: u128 = tests.iter().map(|r| r.elapsed_ms).sum();
        let seconds = (total_ms as f64) / 1000.0;
        out.push_str(&format!(
            "  <testsuite name=\"{}\" tests=\"{n}\" failures=\"{failures}\" errors=\"{errors}\" skipped=\"{skipped}\" time=\"{seconds:.3}\">\n",
            xml_escape(suite)
        ));
        for record in tests {
            let elapsed_s = (record.elapsed_ms as f64) / 1000.0;
            out.push_str(&format!(
                "    <testcase classname=\"{cls}\" name=\"{name}\" time=\"{elapsed_s:.3}\"",
                cls = xml_escape(suite),
                name = xml_escape(&record.name),
            ));
            if matches!(record.status, TestStatus::Ignored | TestStatus::Skipped) {
                let kind = if record.status == TestStatus::Ignored {
                    "ignored"
                } else {
                    "skipped"
                };
                out.push_str(&format!(
                    ">\n      <skipped message=\"{kind}\"/>\n    </testcase>\n"
                ));
            } else if record.passed {
                out.push_str("/>\n");
            } else if record.timed_out {
                out.push_str(">\n      <error type=\"timeout\" message=\"");
                out.push_str(&xml_escape(
                    record.failure_message.as_deref().unwrap_or("timed out"),
                ));
                out.push_str("\"/>\n    </testcase>\n");
            } else {
                out.push_str(">\n      <failure message=\"");
                out.push_str(&xml_escape(
                    record.failure_message.as_deref().unwrap_or("failed"),
                ));
                out.push_str("\"/>\n    </testcase>\n");
            }
        }
        out.push_str("  </testsuite>\n");
    }
    out.push_str("</testsuites>\n");
    out
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod focused_tests {
    use super::*;

    /// The scheduling classifier decides on the source, so its answer is
    /// the same on every run and on every machine.
    #[test]
    fn scheduling_dependence_is_read_from_the_source() {
        let dir = std::env::temp_dir().join(format!("gos-sched-classify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let cases: [(&str, &str, bool); 5] = [
            (
                "goroutine.gos",
                "fn main() {\n    for i in 0..8 { spawn(|| work(i)) }\n}\n",
                true,
            ),
            (
                "spawned.gos",
                "fn main() {\n    let h = spawn(work)\n    let _ = h.join()\n}\n",
                true,
            ),
            (
                "selected.gos",
                "fn main() {\n    select {\n        v = rx.recv() => { let _ = v }\n    }\n}\n",
                true,
            ),
            (
                "sequential.gos",
                "fn main() {\n    for i in 0..8 { println(\"{}\", i) }\n}\n",
                false,
            ),
            (
                "mentions_go_in_a_comment.gos",
                "fn main() {\n    // go through each item in order\n    println(\"1\")\n}\n",
                false,
            ),
        ];
        for (name, source, expected) in cases {
            let path = dir.join(name);
            std::fs::write(&path, source).expect("write fixture");
            assert_eq!(
                tier_parity::output_order_depends_on_scheduling(&path),
                expected,
                "{name} classified wrongly"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn timeout_units_and_zero_are_checked() {
        assert_eq!(
            parse_timeout("250ms").unwrap(),
            std::time::Duration::from_millis(250)
        );
        assert_eq!(
            parse_timeout("2s").unwrap(),
            std::time::Duration::from_secs(2)
        );
        assert!(parse_timeout("0s").is_err());
        assert!(parse_timeout("10").is_err());
    }

    #[test]
    fn parameter_literals_and_shuffle_are_reproducible() {
        let args = parse_test_case_args("1, -2.5, true, \"a\\nline\"").unwrap();
        assert!(
            matches!(args.as_slice(), [gossamer_interp::Value::Int(1), gossamer_interp::Value::Float(value), gossamer_interp::Value::Bool(true), gossamer_interp::Value::String(text)] if *value == -2.5 && text.as_str() == "a\nline")
        );
        let mut first = vec![1, 2, 3, 4, 5];
        let mut second = first.clone();
        deterministic_shuffle(&mut first, 42);
        deterministic_shuffle(&mut second, 42);
        assert_eq!(first, second);
    }
}

/// Extracts fenced code blocks from `//` doc comments and runs each
/// as a standalone program. A block that compiles and executes
/// without panicking passes. Returns a summary; a parse or runtime
/// error counts as a failure but does not abort sibling files.
fn run_doc_tests_in_file(file: &std::path::Path, style: &TestStyle) -> DocTestFileSummary {
    let Ok(source) = fs::read_to_string(file) else {
        return DocTestFileSummary {
            passes: 0,
            failures: 0,
        };
    };
    let tests = extract_doc_tests(&source, &file.display().to_string());
    let mut passes = 0u32;
    let mut failures = 0u32;
    for doc in &tests {
        let body = if doc.code.contains("fn main") {
            doc.code.clone()
        } else {
            format!("fn main() {{\n{}\n}}\n", doc.code)
        };
        let mut map = gossamer_lex::SourceMap::new();
        let file_id = map.add_file(doc.name.clone(), body.clone());
        let Ok((program, tcx)) = load_and_check(&body, file_id, &map) else {
            println!("  {} doc-test {} (compile)", style.fail(), doc.name);
            failures += 1;
            continue;
        };
        let mut vm = gossamer_interp::Vm::new();
        vm.set_source_map(std::sync::Arc::new(map));
        if vm.load(&program, tcx, false).is_err() {
            println!("  {} doc-test {} (compile)", style.fail(), doc.name);
            failures += 1;
            continue;
        }
        vm.clear_source_map();
        match vm.call("main", Vec::new()) {
            Ok(_) => {
                println!("  {} doc-test {}", style.pass(), doc.name);
                passes += 1;
            }
            Err(err) => {
                println!("  {} doc-test {} (runtime): {err}", style.fail(), doc.name);
                failures += 1;
            }
        }
    }
    DocTestFileSummary { passes, failures }
}

/// Extracts every fenced code block enclosed in consecutive `//`
/// doc-comment lines. A blank or non-comment line terminates the
/// enclosing block and drops any open fence. Recognised fence
/// markers: ```` ``` ```` (optionally followed by `gos`). Blocks
/// marked with a different language tag are skipped.
fn extract_doc_tests(source: &str, display: &str) -> Vec<DocTest> {
    let mut out = Vec::new();
    let mut fence: Option<(usize, Vec<String>, bool)> = None;
    for (idx, line) in source.lines().enumerate() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("//") else {
            fence = None;
            continue;
        };
        let body = rest.strip_prefix(' ').unwrap_or(rest);
        let leading = body.trim_start();
        if let Some(after_ticks) = leading.strip_prefix("```") {
            if let Some((open_line, captured, runnable)) = fence.take() {
                if runnable {
                    out.push(DocTest {
                        name: format!("{display}:{open_line}"),
                        code: captured.join("\n"),
                    });
                }
            } else {
                let tag = after_ticks.trim();
                let runnable = tag.is_empty() || tag == "gos" || tag == "gossamer";
                fence = Some((idx + 1, Vec::new(), runnable));
            }
        } else if let Some((_, captured, _)) = fence.as_mut() {
            captured.push(body.to_string());
        }
    }
    out
}

/// Cross-tier walk surface - runs every example through the VM and
/// the LLVM-compiled binary, capturing per-tier outcomes and
/// (optionally) writing the JSON sidecar that
/// `gos feature-status` reads.
pub(crate) mod tier_parity {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::Duration;

    use anyhow::{Result, anyhow};

    use super::TestOpts;
    use crate::cmd::feature_status::{TierStatus, render_sidecar};

    /// Per-tier budget when `--timeout` is not given.
    const DEFAULT_TIER_BUDGET: Duration = Duration::from_mins(1);

    pub(super) fn run(opts: &TestOpts) -> Result<()> {
        let budget = opts.timeout.unwrap_or(DEFAULT_TIER_BUDGET);
        let roots = match opts.path.as_ref() {
            Some(p) => vec![p.clone()],
            None => default_walk_roots(),
        };
        let mut files: Vec<PathBuf> = Vec::new();
        for root in &roots {
            collect_gos(root, &mut files)?;
        }
        files.sort();
        if files.is_empty() {
            return Err(anyhow!(
                "no `.gos` sources found under {}",
                roots
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
            ));
        }
        let mut records: Vec<(String, TierStatus)> = Vec::with_capacity(files.len());
        // A fixture's imports name the stdlib modules it exercises, so a
        // module's row is the aggregate over every fixture that imports it:
        // a tier passes only when all of them pass on it.
        let mut by_module: BTreeMap<String, TierStatus> = BTreeMap::new();
        for file in &files {
            let name = file
                .strip_prefix(workspace_root_or_cwd().unwrap_or_else(|| PathBuf::from(".")))
                .unwrap_or(file)
                .display()
                .to_string();
            let [vm, cranelift, llvm] = evaluate_fixture(file, budget);
            println!(
                "{name}: vm={} cranelift={} llvm={}",
                vm.as_deref().unwrap_or("-"),
                cranelift.as_deref().unwrap_or("-"),
                llvm.as_deref().unwrap_or("-"),
            );
            let status = TierStatus {
                vm,
                cranelift,
                llvm,
            };
            for module in stdlib_modules_used(file) {
                merge_module_status(&mut by_module, module, &status);
            }
            // A language construct earns its tier row the same way a stdlib
            // module does: from a fixture that ran on every tier and
            // actually contains the construct.
            if let Ok(source) = fs::read_to_string(file) {
                for feature in gossamer_std::manifest::feature_status::lang_features_used(&source) {
                    merge_module_status(&mut by_module, feature, &status);
                }
            }
            for feature in gossamer_std::manifest::feature_status::lang_features_pinned(file) {
                merge_module_status(&mut by_module, feature, &status);
            }
            records.push((name, status));
        }
        records.extend(by_module);
        let json = render_sidecar(&records);
        if opts.report.as_deref() == Some("status") {
            let out_path = sidecar_path();
            if let Some(parent) = out_path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            fs::write(&out_path, &json)
                .map_err(|e| anyhow!("writing sidecar {}: {e}", out_path.display()))?;
            println!(
                "feature-status sidecar written to {} ({} records)",
                out_path.display(),
                records.len(),
            );
        } else if opts.report.is_some() {
            return Err(anyhow!("unknown --report value (only `status` supported)"));
        }
        // A tier that reached no verdict is absent from the record rather
        // than marked failing, so only an explicit `fail` counts here.
        let failed = records
            .iter()
            .filter(|(_, s)| {
                [&s.vm, &s.cranelift, &s.llvm]
                    .iter()
                    .any(|t| t.as_deref() == Some("fail"))
            })
            .count();
        let undetermined = records
            .iter()
            .filter(|(_, s)| [&s.vm, &s.cranelift, &s.llvm].iter().any(|t| t.is_none()))
            .count();
        if undetermined > 0 {
            println!(
                "{undetermined} fixture(s) reached no verdict on at least one tier \
                 (a fixture that runs until it is killed exceeds the per-tier budget)"
            );
        }
        if failed == 0 {
            Ok(())
        } else {
            Err(anyhow!(
                "{failed} tier-parity failure(s) - see sidecar for details"
            ))
        }
    }

    fn default_walk_roots() -> Vec<PathBuf> {
        let root = workspace_root_or_cwd().unwrap_or_else(|| PathBuf::from("."));
        vec![root.join("examples"), root.join("feature-testing-examples")]
            .into_iter()
            .filter(|p| p.is_dir())
            .collect()
    }

    fn workspace_root_or_cwd() -> Option<PathBuf> {
        let mut cur = std::env::current_dir().ok()?;
        loop {
            if cur.join("Cargo.toml").exists() && cur.join("crates").is_dir() {
                return Some(cur);
            }
            if !cur.pop() {
                return None;
            }
        }
    }

    fn sidecar_path() -> PathBuf {
        let base = std::env::var_os("CARGO_TARGET_DIR").map_or_else(
            || {
                workspace_root_or_cwd()
                    .map_or_else(|| PathBuf::from("target"), |r| r.join("target"))
            },
            PathBuf::from,
        );
        base.join("debug").join(".feature-status.json")
    }

    fn collect_gos(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        if root.is_file() {
            if root.extension().and_then(|e| e.to_str()) == Some("gos") {
                out.push(root.to_path_buf());
            }
            return Ok(());
        }
        if !root.is_dir() {
            return Ok(());
        }
        for entry in fs::read_dir(root).map_err(|e| anyhow!("read_dir {}: {e}", root.display()))? {
            let entry = entry?;
            collect_gos(&entry.path(), out)?;
        }
        Ok(())
    }

    /// Canonical paths of the stdlib modules `file` imports. Handles both
    /// `use std::path::to::module` and the braced `use std::{a, b::c}`
    /// form; a name the manifest does not declare is ignored.
    fn stdlib_modules_used(file: &Path) -> Vec<String> {
        let Ok(source) = fs::read_to_string(file) else {
            return Vec::new();
        };
        let mut found: Vec<String> = Vec::new();
        for line in source.lines() {
            let Some(rest) = line.trim().strip_prefix("use std::") else {
                continue;
            };
            let rest = rest.split("//").next().unwrap_or(rest).trim();
            let (prefix, leaves) = match rest.split_once('{') {
                Some((prefix, tail)) => (
                    prefix.trim(),
                    tail.trim_end_matches('}').split(',').collect::<Vec<_>>(),
                ),
                None => ("", vec![rest]),
            };
            for leaf in leaves {
                let leaf = leaf.split(" as ").next().unwrap_or(leaf).trim();
                if leaf.is_empty() {
                    continue;
                }
                let path = format!("std::{prefix}{leaf}");
                // A leaf may name an item rather than a module
                // (`use std::sync::channel`); take the module above it.
                let parent = path
                    .rsplit_once("::")
                    .map_or_else(|| path.clone(), |(module, _)| module.to_string());
                for candidate in [path, parent] {
                    if gossamer_std::registry::module(&candidate).is_some() {
                        if !found.contains(&candidate) {
                            found.push(candidate);
                        }
                        break;
                    }
                }
            }
        }
        found
    }

    /// Folds one fixture's outcome into a module's aggregate. `fail` on any
    /// fixture wins over `pass`; a tier with no data anywhere stays absent.
    fn merge_module_status(
        by_module: &mut BTreeMap<String, TierStatus>,
        module: String,
        status: &TierStatus,
    ) {
        let entry = by_module.entry(module).or_default();
        for (slot, incoming) in [
            (&mut entry.vm, &status.vm),
            (&mut entry.cranelift, &status.cranelift),
            (&mut entry.llvm, &status.llvm),
        ] {
            match (slot.as_deref(), incoming.as_deref()) {
                (_, None) => {}
                (None, Some(v)) => *slot = Some(v.to_string()),
                (Some("fail"), _) => {}
                (Some(_), Some(v)) => *slot = Some(v.to_string()),
            }
        }
    }

    /// Runs one fixture on every tier and reduces the result to the
    /// per-tier verdicts the sidecar records.
    fn evaluate_fixture(file: &Path, budget: Duration) -> [Option<String>; 3] {
        // A fixture the VM cannot run to completion - a server, an event
        // loop, anything that exits only when killed - will not complete
        // under the JIT or natively either. Charging it the budget twice
        // more, and building it natively first, buys no information: no
        // tier reaches a verdict, so none can agree.
        let vm_outcome = run_vm(file, budget);
        let mut observed = if matches!(vm_outcome, Outcome::NoVerdict) {
            [vm_outcome, Outcome::NoVerdict, Outcome::NoVerdict]
        } else {
            [
                vm_outcome,
                run_cranelift(file, budget),
                run_llvm(file, budget),
            ]
        };
        for outcome in &mut observed {
            if let Outcome::Ran { stdout, .. } = outcome {
                *stdout = mask_program_path(stdout, file);
            }
        }
        // A fixture whose own output moves between runs cannot be compared
        // on stdout at all - goroutine interleaving, a timestamp, a random
        // seed. Confirm it by re-running the reference tier rather than
        // assuming, and pay for the extra run only when the tiers actually
        // disagreed.
        // Interleaving is a property of the source, not of a sample: two
        // runs that happen to agree prove nothing, so a fixture whose
        // output order is chosen by the scheduler is settled statically and
        // never rests on a race. Re-running the reference still catches the
        // sources of movement no signature describes - a timestamp, a
        // random seed - and is paid for only when the tiers disagreed.
        let mut compare_stdout = !output_order_depends_on_scheduling(file);
        if compare_stdout
            && parity_outcomes(&observed, true)
                .iter()
                .any(|o| o.as_deref() == Some("fail"))
            && let Outcome::Ran { stdout, .. } = &observed[0]
            && let Outcome::Ran { stdout: again, .. } = run_vm(file, budget)
        {
            compare_stdout = *stdout == mask_program_path(&again, file);
        }
        parity_outcomes(&observed, compare_stdout)
    }

    /// Whether the fixture hands the order of its output to the scheduler.
    ///
    /// A concurrent fixture's lines are emitted by whichever goroutine runs
    /// first, so the sequence is one of many correct ones and carries no
    /// cross-tier information. Matching a concurrency form the fixture does
    /// not really use only weakens the comparison to exit status, which is
    /// the conservative direction.
    pub(crate) fn output_order_depends_on_scheduling(file: &Path) -> bool {
        let Ok(source) = fs::read_to_string(file) else {
            return false;
        };
        source.lines().any(|line| {
            let code = line.split("//").next().unwrap_or(line).trim();
            ["spawn(", "select {", "select{"]
                .iter()
                .any(|form| code.contains(form))
        })
    }

    /// Reduces what each tier did into the per-tier verdicts the sidecar
    /// records.
    ///
    /// Parity is agreement between tiers, not success: an example that
    /// deliberately aborts, or one that needs an argument it was not
    /// given, exits non-zero everywhere, and three tiers agreeing on that
    /// is exactly the property this walk exists to prove. Only a tier that
    /// disagrees with the reference - or fails to build at all - is a
    /// failure. The bytecode VM is the reference when it reached a
    /// verdict, otherwise the first tier that did.
    fn parity_outcomes(observed: &[Outcome; 3], compare_stdout: bool) -> [Option<String>; 3] {
        let key = |o: &Outcome| match o {
            Outcome::Ran { exit_code, stdout } => Some((
                *exit_code,
                if compare_stdout {
                    stdout.clone()
                } else {
                    String::new()
                },
            )),
            _ => None,
        };
        let Some(reference) = observed.iter().find_map(key) else {
            return [None, None, None];
        };
        std::array::from_fn(|i| match &observed[i] {
            Outcome::Ran { .. } => {
                if key(&observed[i]).as_ref() == Some(&reference) {
                    Some("pass".to_string())
                } else {
                    Some("fail".to_string())
                }
            }
            // A program that does not build has no run-time behaviour to
            // compare. That is a tier failure only when another tier did
            // run it: a fixture written to be rejected fails everywhere,
            // which is agreement, not divergence.
            Outcome::BuildFailed => (reference.0 == Some(0)).then(|| "fail".to_string()),
            Outcome::NoVerdict | Outcome::Skipped => None,
        })
    }

    /// Replaces the program's own path with a placeholder.
    ///
    /// A program that prints `argv[0]` reports the source path under
    /// `gos run` and the executable's path when compiled. That is the
    /// program correctly naming itself, not the tiers disagreeing.
    fn mask_program_path(text: &str, file: &Path) -> String {
        let mut out = text.to_string();
        let mut needles: Vec<String> = vec![file.display().to_string()];
        if let Some(stem) = file.file_stem().and_then(|s| s.to_str()) {
            needles.push(stem.to_string());
        }
        // Longest first, so a stem inside a full path does not mask half
        // of it and leave the remainder behind.
        needles.sort_by_key(|n| std::cmp::Reverse(n.len()));
        for needle in needles {
            if !needle.is_empty() {
                out = out.replace(&needle, "<program>");
            }
        }
        out
    }

    #[derive(Debug)]
    enum Outcome {
        /// The program ran to completion. The exit code and stdout are
        /// what the tiers have to agree on.
        Ran {
            exit_code: Option<i32>,
            stdout: String,
        },
        /// A native build that did not produce a binary. Unlike a
        /// non-zero exit, this is tier-specific by construction.
        BuildFailed,
        /// The process was still running at the deadline. A server example
        /// runs until it is killed, so exceeding the budget says nothing
        /// about whether the tiers agree; recording it as a failure would
        /// publish every long-running fixture's modules as broken. The
        /// `tier_parity` harness models servers directly (boot, probe,
        /// kill) and remains the gate that catches a real hang.
        NoVerdict,
        /// The tier could not be exercised at all.
        Skipped,
    }

    fn gos_bin() -> PathBuf {
        std::env::current_exe()
            .ok()
            .unwrap_or_else(|| PathBuf::from("gos"))
    }

    /// Runs `file` under `gos run` with the JIT disabled, so this column
    /// reports the bytecode VM alone.
    fn run_vm(file: &Path, budget: Duration) -> Outcome {
        run_interpreted(file, &[("GOS_JIT", "0")], budget)
    }

    /// Runs `file` with the promotion threshold at one call, so every
    /// eligible body reaches Cranelift and this column reports the JIT
    /// rather than a second bytecode run.
    fn run_cranelift(file: &Path, budget: Duration) -> Outcome {
        run_interpreted(file, &[("GOSSAMER_JIT_THRESHOLD", "1")], budget)
    }

    fn run_interpreted(file: &Path, env: &[(&str, &str)], budget: Duration) -> Outcome {
        let Some(sink) = StdoutSink::create(file) else {
            return Outcome::Skipped;
        };
        let mut cmd = Command::new(gos_bin());
        cmd.arg("run")
            .arg(file)
            .stdin(Stdio::null())
            .stdout(sink.stdio())
            .stderr(Stdio::null());
        for (key, value) in env {
            cmd.env(key, value);
        }
        let Ok(child) = cmd.spawn() else {
            return Outcome::Skipped;
        };
        wait_bounded(child, budget, &sink)
    }

    /// A file the child's stdout is redirected into.
    ///
    /// Comparing tiers means reading what each one printed, and a pipe
    /// the parent only drains after the wait would deadlock as soon as a
    /// fixture prints more than the pipe buffer holds.
    struct StdoutSink {
        path: PathBuf,
    }

    impl StdoutSink {
        fn create(file: &Path) -> Option<Self> {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "gos-tier-parity-out-{}-{}-{}.txt",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                file.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("fixture"),
            ));
            fs::File::create(&path).ok()?;
            Some(Self { path })
        }

        fn stdio(&self) -> Stdio {
            fs::OpenOptions::new()
                .write(true)
                .open(&self.path)
                .map_or_else(|_| Stdio::null(), Stdio::from)
        }

        fn read(&self) -> String {
            fs::read_to_string(&self.path).unwrap_or_default()
        }
    }

    impl Drop for StdoutSink {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }

    fn run_llvm(file: &Path, budget: Duration) -> Outcome {
        let scratch = std::env::temp_dir().join(format!(
            "gos-tier-parity-{}-{}",
            std::process::id(),
            file.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown"),
        ));
        let _ = fs::remove_dir_all(&scratch);
        if fs::create_dir_all(&scratch).is_err() {
            return Outcome::Skipped;
        }
        // Debug, not release. The VM checks integer overflow and an
        // optimised build wraps - a profile-dependent difference the
        // language defines on purpose, matching Rust. The debug native
        // build keeps the checking semantics, so it is the build whose
        // arithmetic the VM can be compared against at all. The release
        // pipeline has its own parity groups in `tier_parity`.
        let build = Command::new(gos_bin())
            .arg("build")
            .arg("--out-dir")
            .arg(&scratch)
            .arg(file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let ok = matches!(build, Ok(status) if status.success());
        if !ok {
            let _ = fs::remove_dir_all(&scratch);
            return Outcome::BuildFailed;
        }
        // Find the produced executable (first non-dir, non-`.o` file).
        let mut binary: Option<PathBuf> = None;
        if let Ok(entries) = fs::read_dir(&scratch) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file()
                    && path
                        .extension()
                        .and_then(|e| e.to_str())
                        .is_none_or(|e| !matches!(e, "o" | "ll" | "bc" | "S" | "asm"))
                {
                    binary = Some(path);
                    break;
                }
            }
        }
        let outcome = match (binary, StdoutSink::create(file)) {
            (Some(p), Some(sink)) => {
                let run = Command::new(&p)
                    .stdin(Stdio::null())
                    .stdout(sink.stdio())
                    .stderr(Stdio::null())
                    .spawn();
                match run {
                    // The built executable lives under a scratch path the
                    // source run never sees, so mask it here where it is
                    // known; the caller masks the source path.
                    Ok(child) => match wait_bounded(child, budget, &sink) {
                        Outcome::Ran { exit_code, stdout } => Outcome::Ran {
                            exit_code,
                            stdout: mask_program_path(&stdout, &p),
                        },
                        other => other,
                    },
                    Err(_) => Outcome::BuildFailed,
                }
            }
            _ => Outcome::Skipped,
        };
        let _ = fs::remove_dir_all(&scratch);
        outcome
    }

    /// Grace period before idleness is meaningful: a process still
    /// starting up has not had a chance to burn any CPU yet.
    const IDLE_GRACE: Duration = Duration::from_secs(1);

    /// How long a live process must stay under [`IDLE_TICKS_PER_SEC`]
    /// before it counts as parked. Comfortably above the longest sleep any
    /// fixture takes (100 ms), so a sleeping program is never mistaken for
    /// a parked one.
    const IDLE_WINDOW: Duration = Duration::from_secs(4);

    /// CPU ticks per second under which a live process counts as parked.
    /// The counters run at 100 Hz, so a compute-bound process accrues
    /// about 100 per second per core; a server waiting on its netpoller
    /// accrues well under one, waking only for timers.
    const IDLE_TICKS_PER_SEC: f64 = 5.0;

    fn wait_bounded(
        mut child: std::process::Child,
        timeout: Duration,
        sink: &StdoutSink,
    ) -> Outcome {
        let start = std::time::Instant::now();
        let mut cpu = ProcessCpu::watch(&child);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    return Outcome::Ran {
                        exit_code: status.code(),
                        stdout: sink.read(),
                    };
                }
                Ok(None) => {
                    // A live process consuming no CPU is waiting for
                    // something this walk never supplies - a connection, a
                    // signal, a peer. Waiting out the rest of the budget
                    // cannot change the outcome, so stop early and report
                    // the same no-verdict the deadline would have.
                    let parked = start.elapsed() >= IDLE_GRACE && cpu.is_parked();
                    if parked || start.elapsed() >= timeout {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Outcome::NoVerdict;
                    }
                    std::thread::sleep(Duration::from_millis(40));
                }
                Err(_) => return Outcome::Skipped,
            }
        }
    }

    /// Tracks how long a child has gone without consuming CPU.
    ///
    /// Reads the process-wide user+system tick counters, so a program
    /// blocked on one thread while another computes never reads as idle.
    /// Where the counters are unavailable the watcher reports zero idle
    /// time and the per-tier budget remains the only bound.
    struct ProcessCpu {
        #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
        pid: u32,
        window_ticks: u64,
        window_start: std::time::Instant,
        available: bool,
    }

    impl ProcessCpu {
        fn watch(child: &std::process::Child) -> Self {
            let mut watcher = Self {
                pid: child.id(),
                window_ticks: 0,
                window_start: std::time::Instant::now(),
                available: false,
            };
            if let Some(ticks) = watcher.sample() {
                watcher.window_ticks = ticks;
                watcher.available = true;
            }
            watcher
        }

        /// Cumulative user+system ticks, or `None` where unsupported.
        #[cfg(target_os = "linux")]
        fn sample(&self) -> Option<u64> {
            let stat = std::fs::read_to_string(format!("/proc/{}/stat", self.pid)).ok()?;
            // Fields are space-separated, but the second (comm) may itself
            // contain spaces; it is parenthesised, so resume after the last
            // ')'. utime and stime are fields 14 and 15, counting from 1.
            let rest = &stat[stat.rfind(')')? + 1..];
            let mut fields = rest.split_whitespace().skip(11);
            let utime: u64 = fields.next()?.parse().ok()?;
            let stime: u64 = fields.next()?.parse().ok()?;
            Some(utime + stime)
        }

        #[cfg(not(target_os = "linux"))]
        fn sample(&self) -> Option<u64> {
            None
        }

        /// Whether the process has stayed under the idle rate for a full
        /// window. Each completed window either confirms parking or
        /// restarts the measurement.
        fn is_parked(&mut self) -> bool {
            if !self.available {
                return false;
            }
            let elapsed = self.window_start.elapsed();
            if elapsed < IDLE_WINDOW {
                return false;
            }
            // The process exited between the poll and this read; let the
            // next `try_wait` collect it.
            let Some(ticks) = self.sample() else {
                return false;
            };
            let rate = (ticks - self.window_ticks) as f64 / elapsed.as_secs_f64();
            self.window_ticks = ticks;
            self.window_start = std::time::Instant::now();
            rate < IDLE_TICKS_PER_SEC
        }
    }

    // BTreeMap type import kept to suppress dead-code lint when the
    // sidecar shape is consumed only via render_sidecar.
    #[allow(dead_code)]
    fn _unused_btreemap() -> BTreeMap<String, TierStatus> {
        BTreeMap::new()
    }

    #[cfg(test)]
    mod evidence_tests {
        use super::{default_walk_roots, stdlib_modules_used};
        use std::collections::BTreeSet;

        /// The committed tier-parity evidence names one row per catalog item
        /// a fixture reaches: a stdlib module it imports, and a language
        /// construct it contains.
        ///
        /// The evidence itself is regenerated by the full walk, which runs
        /// every fixture on every tier; this compares only which items it
        /// covers, which is what a fixture gaining or dropping one
        /// invalidates - and it answers in milliseconds where the walk takes
        /// tens of minutes.
        #[test]
        fn committed_evidence_covers_every_module_a_fixture_imports() {
            let mut used: BTreeSet<String> = BTreeSet::new();
            for root in default_walk_roots() {
                let mut files = Vec::new();
                super::collect_gos(&root, &mut files).expect("walk fixtures");
                for file in files {
                    used.extend(stdlib_modules_used(&file));
                    if let Ok(source) = std::fs::read_to_string(&file) {
                        used.extend(gossamer_std::manifest::feature_status::lang_features_used(
                            &source,
                        ));
                    }
                    used.extend(
                        gossamer_std::manifest::feature_status::lang_features_pinned(&file),
                    );
                }
            }
            let recorded: BTreeSet<String> = crate::cmd::feature_status::release_evidence()
                .expect("committed evidence parses")
                .into_keys()
                .collect();
            let missing: Vec<&String> = used.difference(&recorded).collect();
            let extra: Vec<&String> = recorded.difference(&used).collect();
            assert!(
                missing.is_empty() && extra.is_empty(),
                "resources/feature-status.json is stale: missing {missing:?}, extra {extra:?} - \
                 regenerate with `gos test --tier-parity --report status` and copy the std:: \
                 and lang:: rows from target/debug/.feature-status.json"
            );
        }
    }
}
