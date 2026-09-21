//! `gos test --fuzz` - coverage-guided fuzzing of `#[fuzz]` functions.
//!
//! A `#[fuzz]` function takes `&[u8]` and is expected not to panic for
//! any input. The loop picks an entry from the corpus, mutates it, runs
//! it, and keeps the mutation when it reached a branch nothing had
//! reached before - the same counters `gos test --coverage` reports.
//!
//! What makes this worth an agent's time is the last step: a failure is
//! minimised and written into the corpus, and every corpus entry runs
//! under plain `gos test`. A crash arrives as a deterministic test that
//! fails until it is fixed, not as a report someone has to transcribe.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use gossamer_interp::Value;

use crate::cmd::attr_walk::item_has_attr;
use crate::paths::read_source;

/// Directory holding a fuzz target's corpus, relative to the source's
/// directory: `testdata/fuzz/<target>/`.
const CORPUS_DIR: &str = "testdata/fuzz";

/// Inputs tried before the loop gives up on finding new coverage, when
/// no explicit duration is set.
const DEFAULT_ITERATIONS: u64 = 10_000;

/// A discovered fuzz target.
pub(crate) struct FuzzTarget {
    /// Function name as declared.
    pub name: String,
    /// File the function lives in.
    pub file: PathBuf,
}

/// Outcome of running one input.
enum Run {
    Ok,
    /// The program faulted; the string is the report.
    Crash(String),
}

/// Finds every `#[fuzz]` function under `path`.
pub(crate) fn discover(files: &[PathBuf]) -> Result<Vec<FuzzTarget>> {
    let mut out = Vec::new();
    for file in files {
        let source = read_source(file)?;
        let mut map = gossamer_lex::SourceMap::new();
        let file_id = map.add_file(file.to_string_lossy().into_owned(), source.clone());
        let (sf, _) = gossamer_parse::parse_source_file(&source, file_id);
        for item in &sf.items {
            if let gossamer_ast::ItemKind::Fn(decl) = &item.kind
                && item_has_attr(item, "fuzz")
            {
                out.push(FuzzTarget {
                    name: decl.name.name.clone(),
                    file: file.clone(),
                });
            }
        }
    }
    Ok(out)
}

/// Corpus directory for `target`, beside the source that declares it.
pub(crate) fn corpus_dir(target: &FuzzTarget) -> PathBuf {
    target
        .file
        .parent()
        .unwrap_or(Path::new("."))
        .join(CORPUS_DIR)
        .join(&target.name)
}

/// Every committed corpus entry for `target`, sorted for determinism.
pub(crate) fn corpus_entries(target: &FuzzTarget) -> Vec<PathBuf> {
    let dir = corpus_dir(target);
    let Ok(read) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut entries: Vec<PathBuf> = read
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    entries.sort();
    entries
}

/// Loads `file` into a VM ready to call fuzz targets in it.
fn load(file: &Path) -> Result<gossamer_interp::Vm> {
    let source = read_source(file)?;
    let augmented = gossamer_parse::autoderive::augment_source(&source);
    let mut map = gossamer_lex::SourceMap::new();
    let file_id = map.add_file(file.to_string_lossy().into_owned(), augmented.clone());
    let (program, _sf, tcx) = crate::loaders::load_and_check_with_sf(&augmented, file_id, &map)
        .map_err(|_| anyhow!("{}: does not check; fix it before fuzzing", file.display()))?;
    let mut vm = gossamer_interp::Vm::new();
    vm.set_source_map(Arc::new(map));
    vm.load(&program, tcx, false)
        .map_err(|e| anyhow!("loading {}: {e}", file.display()))?;
    Ok(vm)
}

/// Runs one input through `target`.
fn run_input(vm: &mut gossamer_interp::Vm, name: &str, input: &[u8]) -> Run {
    let arg = Value::ByteVec(Arc::new(input.to_vec()));
    match vm.call(name, vec![arg]) {
        Ok(_) => Run::Ok,
        Err(e) => Run::Crash(e.to_string()),
    }
}

/// Branch identities covered by the run that just finished.
fn coverage_keys() -> HashSet<(String, u32, u32)> {
    gossamer_runtime::coverage::snapshot()
        .into_iter()
        .filter(|c| c.hits > 0)
        .map(|c| (c.file, c.line, c.branch))
        .collect()
}

/// Deterministic bit-mixer. The loop must be reproducible from a seed:
/// a fuzzer nobody can replay is a fuzzer whose findings cannot be
/// investigated.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next() % bound as u64) as usize
        }
    }
}

/// Values that disproportionately find bugs: boundaries of the integer
/// widths, and the bytes that delimit text formats.
const INTERESTING: &[u8] = &[0, 1, 0x7f, 0x80, 0xff, b'\n', b'"', b'\\', b'{', b'0'];

/// Produces a mutated copy of `seed`.
fn mutate(seed: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut out = seed.to_vec();
    match rng.below(6) {
        // Flip one bit.
        0 if !out.is_empty() => {
            let i = rng.below(out.len());
            out[i] ^= 1 << rng.below(8);
        }
        // Replace one byte with an interesting value.
        1 if !out.is_empty() => {
            let i = rng.below(out.len());
            out[i] = INTERESTING[rng.below(INTERESTING.len())];
        }
        // Insert a byte.
        2 => {
            let i = rng.below(out.len() + 1);
            out.insert(i, INTERESTING[rng.below(INTERESTING.len())]);
        }
        // Remove a byte.
        3 if !out.is_empty() => {
            let i = rng.below(out.len());
            out.remove(i);
        }
        // Duplicate a run, which is how length-prefix and nesting bugs
        // surface.
        4 if !out.is_empty() => {
            let start = rng.below(out.len());
            let len = 1 + rng.below(out.len() - start);
            let chunk: Vec<u8> = out[start..start + len].to_vec();
            let at = rng.below(out.len() + 1);
            for (offset, byte) in chunk.into_iter().enumerate() {
                out.insert(at + offset, byte);
            }
        }
        // Truncate.
        _ => {
            if out.is_empty() {
                out.push(INTERESTING[rng.below(INTERESTING.len())]);
            } else {
                out.truncate(rng.below(out.len()));
            }
        }
    }
    // An unbounded corpus entry slows every later iteration for no gain.
    out.truncate(4096);
    out
}

/// Shrinks `input` while it still fails, so the committed regression is
/// the smallest input that reproduces.
///
/// Delta debugging: try removing progressively smaller chunks, then
/// individual bytes, keeping any smaller input that still fails.
fn minimise(vm: &mut gossamer_interp::Vm, name: &str, input: &[u8]) -> Vec<u8> {
    let mut best = input.to_vec();
    let mut chunk = best.len() / 2;
    while chunk > 0 {
        let mut position = 0;
        while position < best.len() {
            let mut candidate = best.clone();
            let end = (position + chunk).min(candidate.len());
            candidate.drain(position..end);
            if !candidate.is_empty() && matches!(run_input(vm, name, &candidate), Run::Crash(_)) {
                best = candidate;
            } else {
                position += chunk;
            }
        }
        chunk /= 2;
    }
    best
}

/// Runs the fuzz loop for every target in `targets`.
pub(crate) fn run(targets: &[FuzzTarget], duration: Option<Duration>, seed: u64) -> Result<()> {
    if targets.is_empty() {
        return Err(anyhow!("no `#[fuzz]` functions found"));
    }
    gossamer_runtime::coverage::set_enabled(true);
    let mut failures = 0usize;
    for target in targets {
        outln!("fuzz: {} ({})", target.name, target.file.display());
        let mut vm = load(&target.file)?;

        // Corpus first: an entry earns its place by covering something,
        // and a committed crash must fail immediately rather than after
        // a mutation happens to rediscover it.
        let mut corpus: Vec<Vec<u8>> = corpus_entries(target)
            .iter()
            .filter_map(|p| std::fs::read(p).ok())
            .collect();
        if corpus.is_empty() {
            corpus.push(b"gossamer".to_vec());
        }
        let mut covered: HashSet<(String, u32, u32)> = HashSet::new();
        for entry in &corpus {
            if let Run::Crash(report) = run_input(&mut vm, &target.name, entry) {
                outln!("  corpus entry still fails: {report}");
                failures += 1;
            }
            covered.extend(coverage_keys());
        }

        let started = Instant::now();
        let mut rng = Rng(seed);
        let mut executed = 0u64;
        let mut found = 0usize;
        let crash = loop {
            let over = match duration {
                Some(limit) => started.elapsed() >= limit,
                None => executed >= DEFAULT_ITERATIONS,
            };
            if over {
                break None;
            }
            executed += 1;
            let base = &corpus[rng.below(corpus.len())];
            let candidate = mutate(base, &mut rng);
            gossamer_runtime::coverage::reset();
            if let Run::Crash(report) = run_input(&mut vm, &target.name, &candidate) {
                break Some((candidate, report));
            }
            let keys = coverage_keys();
            if !keys.is_subset(&covered) {
                covered.extend(keys);
                corpus.push(candidate);
                found += 1;
            }
        };

        match crash {
            Some((input, report)) => {
                let minimal = minimise(&mut vm, &target.name, &input);
                let path = write_crash(target, &minimal)?;
                outln!(
                    "  crash after {executed} input(s): {report}\n  \
                     minimised {} -> {} byte(s), written to {}\n  \
                     it now runs as a regression under `gos test`",
                    input.len(),
                    minimal.len(),
                    path.display(),
                );
                failures += 1;
            }
            None => outln!("  {executed} input(s), {found} added to the corpus, no crash"),
        }
    }
    gossamer_runtime::coverage::set_enabled(false);
    if failures > 0 {
        return Err(anyhow!("{failures} fuzz target(s) failed"));
    }
    Ok(())
}

/// Writes a minimised failing input into the corpus, named by content so
/// re-finding the same crash does not add a second copy.
fn write_crash(target: &FuzzTarget, input: &[u8]) -> Result<PathBuf> {
    let dir = corpus_dir(target);
    std::fs::create_dir_all(&dir)?;
    let digest = gossamer_pkg::sha256::hex(input);
    let path = dir.join(format!("crash-{}", &digest[..16]));
    std::fs::write(&path, input)?;
    Ok(path)
}

/// Runs every corpus entry for every target, as ordinary tests.
///
/// This is what turns a fuzz finding into a gate: once a crash is
/// committed, plain `gos test` fails until it is fixed.
pub(crate) fn run_corpus_as_tests(targets: &[FuzzTarget]) -> Result<(usize, usize)> {
    let mut passed = 0usize;
    let mut failed = 0usize;
    for target in targets {
        let entries = corpus_entries(target);
        if entries.is_empty() {
            continue;
        }
        let mut vm = load(&target.file)?;
        for entry in entries {
            let Ok(input) = std::fs::read(&entry) else {
                continue;
            };
            match run_input(&mut vm, &target.name, &input) {
                Run::Ok => passed += 1,
                Run::Crash(report) => {
                    failed += 1;
                    outln!(
                        "FAIL {}::{} [{}]: {report}",
                        target.file.display(),
                        target.name,
                        entry.file_name().unwrap_or_default().to_string_lossy(),
                    );
                }
            }
        }
    }
    Ok((passed, failed))
}
