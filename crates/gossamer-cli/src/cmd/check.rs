//! `gos check [PATH]` - parse + resolve + typecheck + exhaustiveness.
//!
//! Walks `<project-root>/src` when invoked with no path; honours a
//! single file or a directory when supplied. Renders every stage's
//! diagnostics through the shared renderer, surfaces a non-zero exit
//! when any stage produces error-severity output.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::loaders::timings_line;
use crate::paths::{
    collect_lint_targets, default_test_root, friendly_io_error, resolve_project_entry,
    stderr_supports_colour,
};

/// How one `gos check` reports and what it does besides checking.
pub(crate) struct CheckOptions {
    /// Print per-stage timings on success.
    pub timings: bool,
    /// Diagnostic output format.
    pub message_format: crate::cli::MessageFormat,
    /// Apply machine-applicable suggestions.
    pub fix: bool,
    /// Where to write the frontend blob of an accepted unit.
    pub emit_frontend_blob: Option<PathBuf>,
}

/// `gos check` dispatcher: routes between single-file and
/// whole-project walks.
pub(crate) fn dispatch(path: Option<PathBuf>, options: &CheckOptions) -> Result<()> {
    if let Err(err) = crate::binding_dispatch::ensure_external_signatures() {
        eprintln!("warning: failed to load rust-binding signatures: {err}");
    }
    let resolved = match path {
        Some(p) => p,
        None => default_test_root()?,
    };
    let meta = fs::metadata(&resolved).map_err(|e| friendly_io_error(e, &resolved))?;
    if meta.is_file() {
        // A module of a package is checked as part of that package, through
        // its entry: `crate::`, `super::`, and a bare sibling module name
        // each name what they name when the whole package compiles, which is
        // the unit `gos run` and `gos build` assemble. A loose file and an
        // integration test under `tests/` are their own roots.
        let target = crate::paths::enclosing_project_entry(&resolved).unwrap_or(resolved);
        return run(&target, options);
    }
    // A project directory is checked as one bundled unit (the entry plus
    // its auto-bundled sibling / subdirectory modules), so cross-module
    // references resolve exactly as they do under `gos` / `gos
    // build`. Without this, each file is type-checked in isolation and a
    // valid `crate::other::item` call reports a false unresolved-name
    // error. A directory without a single resolvable entry falls back to
    // the per-file sweep below.
    if let Ok(entry) = resolve_project_entry(&resolved) {
        return run(&entry, options);
    }
    let files = collect_lint_targets(&resolved)?;
    if files.is_empty() {
        return Err(anyhow!(
            "no `.gos` sources found under {}",
            resolved.display()
        ));
    }
    // A directory that is not itself a project may still hold several. A
    // file inside one is checked as part of that project, once, through its
    // entry: checking it alone would report the cross-module references its
    // siblings supply as unresolved names.
    let files = crate::paths::group_targets_by_project(&files);
    let mut total_errors = 0u32;
    for file in &files {
        if files.len() > 1 {
            outln!("=== {} ===", file.display());
        }
        match run(file, options) {
            Ok(()) => {}
            Err(err) => {
                eprintln!("{err}");
                total_errors += 1;
            }
        }
    }
    if total_errors > 0 {
        return Err(anyhow!(
            "check: {total_errors} {file_word} failed across {} source(s)",
            files.len(),
            file_word = if total_errors == 1 { "file" } else { "files" },
        ));
    }
    outln!(
        "check: {n} {file_word} ok",
        n = files.len(),
        file_word = if files.len() == 1 { "file" } else { "files" },
    );
    Ok(())
}

/// Single-file `gos check`. Public to the crate so the dispatcher
/// above and the `cmd::watch` re-runner can share it.
///
/// A verdict is a function of the unit's text, the files its comptime
/// regions read, and how it is rendered, so an unchanged unit replays the
/// output it produced last time instead of checking again. `--timings`
/// and `--fix` always do the work, as measuring and rewriting it is their
/// purpose.
pub(crate) fn run(file: &Path, options: &CheckOptions) -> Result<()> {
    let CheckOptions {
        timings,
        message_format,
        fix,
        ref emit_frontend_blob,
    } = *options;
    let unit = crate::paths::read_entry_unit(file)?;
    let colour = stderr_supports_colour();
    // Emitting a blob needs the frontend to run, which a replayed verdict
    // skips.
    let emits =
        emit_frontend_blob.is_some() || std::env::var_os("GOS_EMIT_FRONTEND_BLOB").is_some();
    let key = (!timings && !fix && !emits && gossamer_driver::cache_enabled())
        .then(|| verdict_key(file, &unit, message_format, colour))
        .flatten();
    if let Some(key) = &key
        && let Some(verdict) = gossamer_driver::load_blob::<Verdict>(key)
        && crate::unit_cache::inputs_unchanged(&verdict.inputs).is_some()
    {
        gossamer_driver::trace_cache_hit("check verdict replayed", key);
        verdict.transcript.replay();
        return verdict_result(verdict.errors);
    }
    let mut transcript = Transcript::default();
    let checked = check_unit(file, unit, options, colour, &mut transcript);
    transcript.replay();
    match checked? {
        Checked::Rewritten => run(
            file,
            &CheckOptions {
                fix: false,
                emit_frontend_blob: emit_frontend_blob.clone(),
                ..*options
            },
        ),
        Checked::Verdict { errors, inputs } => {
            if let Some(key) = &key
                && let Some(inputs) = crate::unit_cache::input_lines(&inputs)
            {
                gossamer_driver::store_blob(
                    key,
                    &Verdict {
                        inputs,
                        transcript,
                        errors,
                    },
                );
            }
            verdict_result(errors)
        }
    }
}

/// What `gos check` printed for a unit, in the order each stream saw it.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Transcript {
    stdout: String,
    stderr: String,
}

impl Transcript {
    fn out(&mut self, line: std::fmt::Arguments<'_>) {
        let _ = writeln!(self.stdout, "{line}");
    }

    fn err(&mut self, text: std::fmt::Arguments<'_>) {
        let _ = write!(self.stderr, "{text}");
    }

    /// Writes the recorded output to the process's own streams, stderr
    /// first as diagnostics precede the summary line.
    fn replay(&self) {
        if !self.stderr.is_empty() {
            eprint!("{}", self.stderr);
        }
        if !self.stdout.is_empty() {
            crate::write_stdout(format_args!("{}", self.stdout));
        }
    }
}

/// A check's recorded outcome: what it printed, how many errors it found,
/// and the stamp lines of the files its comptime regions read.
#[derive(Debug, Serialize, Deserialize)]
struct Verdict {
    inputs: Vec<String>,
    transcript: Transcript,
    errors: usize,
}

/// How a check that ran to its verdict ended.
enum Checked {
    /// The front end answered, with this many errors, after the comptime
    /// fold read `inputs`.
    Verdict { errors: usize, inputs: Vec<PathBuf> },
    /// `--fix` rewrote the unit, which is checked afresh.
    Rewritten,
}

fn verdict_result(errors: usize) -> Result<()> {
    if errors == 0 {
        Ok(())
    } else {
        Err(anyhow!("check failed with {errors} diagnostic(s)"))
    }
}

/// The key of `file`'s verdict: the unit's text, where each region of it
/// came from, the entry's own text the lints read, and how diagnostics
/// render. `None` when the entry cannot be read, which the check reports.
fn verdict_key(
    file: &Path,
    unit: &crate::paths::EntryUnit,
    message_format: crate::cli::MessageFormat,
    colour: bool,
) -> Option<gossamer_driver::FrontendCacheKey> {
    let entry_text = crate::paths::read_source(file).ok()?;
    let label = format!(
        "path={}|format={message_format:?}|colour={colour}|entry={}|origins={:?}|lint={}",
        std::path::absolute(file)
            .unwrap_or_else(|_| file.to_path_buf())
            .display(),
        unit.entry.display(),
        unit.origins,
        entry_text,
    );
    Some(gossamer_driver::unit_key(
        &unit.source,
        "check-verdict",
        &label,
    ))
}

fn check_unit(
    file: &Path,
    unit: crate::paths::EntryUnit,
    options: &CheckOptions,
    colour: bool,
    transcript: &mut Transcript,
) -> Result<Checked> {
    let CheckOptions {
        timings,
        message_format,
        fix,
        ref emit_frontend_blob,
    } = *options;
    let user_source = unit.source;
    // Augment with the serde stubs (`__gos_serde_*`) and derives so
    // `to_json::<T>(..)` / `from_json::<T>(..)` resolve, exactly as
    // `gos` / `gos build` do before reaching the source map. The comptime
    // fold makes `gos check` authoritative for comptime: a region that is
    // not compile-time-known is reported here, not deferred to `run` /
    // `build`. The fold runs the program, so a failure is reported only
    // once the front end has accepted the unfolded unit - the order the
    // editor uses - and the front end's warnings are reported alongside it.
    let prepared = crate::unit_cache::prepare(&user_source, &file.to_string_lossy());
    let source = prepared.source;
    let mut map = gossamer_lex::SourceMap::new();
    let file_id = map.add_file(file.to_string_lossy().into_owned(), source.clone());
    crate::paths::register_unit_origins(&mut map, file_id, &unit.entry, &unit.origins);
    crate::paths::register_generated_tail(&mut map, file_id, prepared.generated_len);
    let render_opts = gossamer_diagnostics::RenderOptions { colour };
    // `check` runs the same authoritative front-end gate as `run` /
    // `build` / `test` / `bench`: one parse + resolve + typecheck +
    // exhaustiveness pass under a single fatal-error policy. Sharing the
    // gate is what keeps `check` from drifting looser than the tiers it
    // is meant to guard.
    let outcome = gossamer_driver::check_frontend_with_program(&source, file_id, prepared.program);
    if let Some(path) = emit_frontend_blob {
        gossamer_driver::write_frontend_blob(&outcome, path)
            .map_err(|e| anyhow!("--emit-frontend-blob {}: {e}", path.display()))?;
    }
    if fix {
        // A unit is many files, and a rewrite lands in whichever ones
        // carried the diagnostics. Naming only the entry leaves a reader
        // with no reason to look at the others.
        let mut edited: BTreeMap<PathBuf, usize> = BTreeMap::new();
        let applied = apply_suggestions(
            file,
            file_id,
            &map,
            &user_source,
            &source,
            &outcome.diagnostics,
            &mut edited,
        )?;
        if edited.is_empty() {
            transcript.out(format_args!("fix: 0 edit(s) applied"));
        } else {
            for (path, count) in &edited {
                transcript.out(format_args!(
                    "fix: {count} edit(s) applied to {}",
                    display_path(path)
                ));
            }
            if edited.len() > 1 {
                transcript.out(format_args!(
                    "fix: {applied} edit(s) across {} file(s)",
                    edited.len()
                ));
            }
            // The verdict belongs to the source as rewritten: what the edits
            // resolved is gone, and what remains is reported afresh.
            return Ok(Checked::Rewritten);
        }
    }
    for diag in outcome.diagnostics.iter().chain(&outcome.warnings) {
        emit_diag(diag, &map, render_opts, message_format, transcript);
    }
    if let Some(err) = prepared.fold_error
        && outcome.diagnostics.is_empty()
    {
        return Err(err);
    }
    // The editor and `gos lint` both run the default lint registry, so
    // `check` runs it too and stays the superset gate. Lints are advisory
    // here: they report at warning severity and never move the exit code,
    // which `gos lint --deny-warnings` remains the gate for.
    for diag in lint_diagnostics(file, &mut map)? {
        emit_diag(&diag, &map, render_opts, message_format, transcript);
    }
    if outcome.diagnostics.is_empty() {
        transcript.out(format_args!(
            "check: ok ({} items typed)",
            outcome.checked.sf.items.len()
        ));
        if timings {
            transcript.out(format_args!(
                "{}",
                timings_line(
                    source.len(),
                    prepared.autoderive,
                    prepared.comptime,
                    prepared.reused,
                    &outcome.timings,
                )
            ));
        }
    }
    Ok(Checked::Verdict {
        errors: outcome.diagnostics.len(),
        inputs: prepared.comptime_inputs,
    })
}

/// Runs the default lint registry over `path`'s own text and returns
/// its findings at warning severity.
///
/// The findings are anchored to a second entry in `map` holding that
/// text alone: the checked source carries the project bundle, the
/// synthesized autoderive tail, and any comptime splices, and a lint
/// span is only meaningful against the text the author wrote. Sibling
/// modules are linted when `gos lint` walks them under their own paths.
fn lint_diagnostics(
    path: &Path,
    map: &mut gossamer_lex::SourceMap,
) -> Result<Vec<gossamer_diagnostics::Diagnostic>> {
    let source = crate::paths::read_source(path)?;
    let lint_file = map.add_file(path.to_string_lossy().into_owned(), source.clone());
    let (sf, parse_diags) = gossamer_parse::parse_source_file(&source, lint_file);
    if !parse_diags.is_empty() {
        return Ok(Vec::new());
    }
    let mut registry = gossamer_lint::Registry::with_defaults();
    gossamer_lint::apply_attributes(&sf.attrs, &mut registry);
    let mut diagnostics = gossamer_lint::run(&sf, &source, &registry);
    for diag in &mut diagnostics {
        diag.severity = gossamer_diagnostics::Severity::Warning;
    }
    Ok(diagnostics)
}

/// Renders a structured diagnostic for stderr, branching on
/// `--message-format`. Plain mode uses the colour-aware text
/// frame; JSON mode writes the single-line JSON object directly
/// (the trailing newline is part of `render_json`'s output).
fn emit_diag(
    structured: &gossamer_diagnostics::Diagnostic,
    map: &gossamer_lex::SourceMap,
    render_opts: gossamer_diagnostics::RenderOptions,
    message_format: crate::cli::MessageFormat,
    transcript: &mut Transcript,
) {
    match message_format {
        crate::cli::MessageFormat::Plain => {
            transcript.err(format_args!(
                "{}\n",
                gossamer_diagnostics::render(structured, map, render_opts)
            ));
        }
        crate::cli::MessageFormat::Json => {
            // Single-line JSON envelope; `render_json` adds the
            // trailing `\n` so consumers can read line-delimited.
            transcript.err(format_args!(
                "{}",
                gossamer_diagnostics::render_json(structured, map)
            ));
        }
    }
}

/// Applies the machine-applicable rewrites for `file` and returns how
/// many landed: first the suggestions the diagnostics carry, then the
/// lint fixes `gos lint --fix` would apply, each verified against a
/// re-check of the whole unit.
///
/// The two sources are applied in rounds rather than merged. A lint fix
/// is derived from what the source means, and a source that still holds
/// an unresolved name means something else: an undefined-variable error
/// makes its intended binding look unused, so a merged pass would rename
/// the binding and the use apart.
///
/// A suggestion addresses the file its bytes were written in. The
/// checked text is the project bundle plus the synthesized autoderive
/// tail and any comptime splices, so a span is first bounded to the
/// bundle - the longest common prefix of the two texts - and then
/// resolved through the unit's provenance to a sibling module's own
/// offsets, exactly as the diagnostic that carried it was rendered. A
/// span the bundle does not cover, or one straddling two files, is left
/// for the author.
fn apply_suggestions(
    file: &Path,
    unit: gossamer_lex::FileId,
    map: &gossamer_lex::SourceMap,
    user_source: &str,
    checked_source: &str,
    diagnostics: &[gossamer_diagnostics::Diagnostic],
    edited: &mut BTreeMap<PathBuf, usize>,
) -> Result<usize> {
    let safe_len = common_prefix_len(user_source, checked_source).min(user_source.len());
    let mut applied = 0usize;

    let mut by_file: BTreeMap<PathBuf, Vec<gossamer_lint::Fix>> = BTreeMap::new();
    for suggestion in diagnostics.iter().flat_map(|diag| &diag.suggestions) {
        let span = suggestion.location.span;
        if span.end as usize > safe_len {
            continue;
        }
        let (start_file, start) = map.origin_of(unit, span.start);
        // The last byte says which file a non-empty span ends in; an
        // insertion has no bytes and sits where it starts.
        let (end_file, end) = if span.end > span.start {
            let (end_file, last) = map.origin_of(unit, span.end - 1);
            (end_file, last + 1)
        } else {
            (start_file, start)
        };
        if start_file != end_file {
            continue;
        }
        by_file
            .entry(PathBuf::from(map.file_name(start_file)))
            .or_default()
            .push(gossamer_lint::Fix {
                span: gossamer_lex::Span::new(start_file, start, end),
                replacement: suggestion.replacement.clone(),
                lint_id: "diagnostic",
            });
    }
    if !by_file.is_empty() {
        let originals = by_file
            .keys()
            .map(|path| Ok((path.clone(), crate::paths::read_source(path)?)))
            .collect::<Result<Vec<(PathBuf, String)>>>()?;
        let mut candidates = Vec::with_capacity(originals.len());
        let mut edits = 0usize;
        for (path, original) in &originals {
            let candidate = gossamer_lint::apply_fixes(original, &by_file[path]);
            if candidate != *original {
                edits += by_file[path].len();
            }
            candidates.push((path.clone(), candidate));
        }
        if edits > 0 {
            let before = recheck_codes(file);
            write_all(&candidates)?;
            let after = recheck_codes(file);
            let better = match (&before, &after) {
                (Some(before), Some(after)) => is_an_improvement(before, after),
                _ => false,
            };
            if better {
                applied += edits;
                for (path, original) in &originals {
                    let fixes = by_file[path].len();
                    if fixes > 0
                        && candidates
                            .iter()
                            .any(|(candidate, text)| candidate == path && text != original)
                    {
                        *edited.entry(path.clone()).or_default() += fixes;
                    }
                }
            } else {
                write_all(&originals)?;
            }
        }
    }

    let current = crate::paths::read_source(file)?;
    let lint_fixes = lint_fixes_for(file, &current);
    if !lint_fixes.is_empty() {
        let candidate = gossamer_lint::apply_fixes(&current, &lint_fixes);
        if candidate != current {
            let baseline = recheck(file);
            fs::write(file, &candidate).map_err(|e| friendly_io_error(e, file))?;
            if recheck(file) <= baseline {
                applied += lint_fixes.len();
                *edited.entry(file.to_path_buf()).or_default() += lint_fixes.len();
            } else {
                fs::write(file, &current).map_err(|e| friendly_io_error(e, file))?;
            }
        }
    }
    Ok(applied)
}

/// `path` written the way the caller named the unit: relative to the
/// current directory when it is under it, absolute otherwise. A report
/// that mixed the two spellings for files of one unit reads as though
/// they came from different places.
fn display_path(path: &Path) -> String {
    let Ok(cwd) = std::env::current_dir() else {
        return path.display().to_string();
    };
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    match absolute.strip_prefix(&cwd) {
        Ok(relative) => Path::new(".").join(relative).display().to_string(),
        Err(_) => absolute.display().to_string(),
    }
}

/// Writes each `(path, text)` pair back to disk.
fn write_all(texts: &[(PathBuf, String)]) -> Result<()> {
    for (path, text) in texts {
        fs::write(path, text).map_err(|e| friendly_io_error(e, path))?;
    }
    Ok(())
}

/// Auto-applicable lint edits for `source` read as `file`'s text, under
/// the registry its own attributes configure.
fn lint_fixes_for(file: &Path, source: &str) -> Vec<gossamer_lint::Fix> {
    let mut map = gossamer_lex::SourceMap::new();
    let id = map.add_file(file.to_string_lossy().into_owned(), source.to_string());
    let (sf, parse_diags) = gossamer_parse::parse_source_file(source, id);
    if !parse_diags.is_empty() {
        return Vec::new();
    }
    let mut registry = gossamer_lint::Registry::with_defaults();
    gossamer_lint::apply_attributes(&sf.attrs, &mut registry);
    gossamer_lint::fixes(&sf, &registry, source)
}

/// Error count the front-end gate reports for the unit `file` assembles
/// from disk as it now stands.
///
/// The unit is re-bundled rather than re-read as one file: a rewrite in a
/// sibling module is only proven by checking the entry with that module
/// in place, and an entry checked alone reports every sibling's name as
/// unresolved.
fn recheck(file: &Path) -> usize {
    recheck_codes(file).map_or(usize::MAX, |codes| codes.len())
}

/// Every diagnostic code the front-end gate reports for the unit `file`
/// assembles from disk as it now stands, as a multiset.
///
/// `None` when the unit cannot be read or folded at all, which is a
/// worse outcome than any diagnostic and is never an improvement.
fn recheck_codes(file: &Path) -> Option<BTreeMap<String, usize>> {
    let unit = crate::paths::read_entry_unit(file).ok()?;
    let prepared = crate::unit_cache::prepare(&unit.source, &file.to_string_lossy());
    if prepared.fold_error.is_some() {
        return None;
    }
    let folded = prepared.source;
    let mut map = gossamer_lex::SourceMap::new();
    let id = map.add_file(file.to_string_lossy().into_owned(), folded.clone());
    let outcome = gossamer_driver::check_frontend(&folded, id);
    let mut codes: BTreeMap<String, usize> = BTreeMap::new();
    for diag in &outcome.diagnostics {
        *codes.entry(diag.code.to_string()).or_default() += 1;
    }
    Some(codes)
}

/// Whether rewriting turned `before` into `after` without introducing a
/// diagnostic the program did not already have.
///
/// A count alone is the wrong test: a batch that trades three style
/// diagnostics for one type error reports a lower number and is a
/// regression. What has to hold is that every code still reported was
/// already reported, and that there are fewer of them. A source that does
/// not parse reports nothing past its parse errors, so there only the parse
/// errors were observed, and the later diagnostics a repair unmasks are
/// ones the program already had.
fn is_an_improvement(before: &BTreeMap<String, usize>, after: &BTreeMap<String, usize>) -> bool {
    let parse_stage = |codes: &BTreeMap<String, usize>| -> BTreeMap<String, usize> {
        codes
            .iter()
            .filter(|(code, _)| code.starts_with("GP"))
            .map(|(code, count)| (code.clone(), *count))
            .collect()
    };
    let (before_parse, after_parse) = (parse_stage(before), parse_stage(after));
    if before_parse.is_empty() {
        is_a_strict_submultiset(after, before)
    } else {
        is_a_strict_submultiset(&after_parse, &before_parse)
    }
}

/// Whether `smaller` holds fewer diagnostics than `larger` and no code more
/// often than `larger` does.
fn is_a_strict_submultiset(
    smaller: &BTreeMap<String, usize>,
    larger: &BTreeMap<String, usize>,
) -> bool {
    let total = |codes: &BTreeMap<String, usize>| codes.values().sum::<usize>();
    total(smaller) < total(larger)
        && smaller
            .iter()
            .all(|(code, count)| larger.get(code).is_some_and(|had| count <= had))
}

/// Byte length of the longest common prefix of `a` and `b`, rounded down
/// to a character boundary so a span slice stays valid UTF-8.
fn common_prefix_len(a: &str, b: &str) -> usize {
    let mut len = a
        .as_bytes()
        .iter()
        .zip(b.as_bytes())
        .take_while(|(x, y)| x == y)
        .count();
    while len > 0 && !a.is_char_boundary(len) {
        len -= 1;
    }
    len
}
