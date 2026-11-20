//! `gos lint [PATH] [--deny-warnings] [--explain ID] [--fix]` -
//! runs the lint suite and applies / reports findings.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

use crate::paths::{collect_lint_targets, default_test_root, read_source, stderr_supports_colour};

/// `gos lint` dispatcher: walks the project root when no path is
/// supplied.
pub(crate) fn dispatch(
    path: Option<PathBuf>,
    deny_warnings: bool,
    explain: Option<&str>,
    fix: bool,
) -> Result<()> {
    let resolved = match path {
        Some(p) => p,
        None => default_test_root()?,
    };
    run(&resolved, deny_warnings, explain, fix)
}

fn run(path: &PathBuf, deny_warnings: bool, explain: Option<&str>, fix: bool) -> Result<()> {
    if let Some(id) = explain {
        // A diagnostic prints the code, so `--explain` takes it as well as the name.
        let id = gossamer_lint::lint_for_code(id).unwrap_or(id);
        match gossamer_lint::lint_explanation(id) {
            Some(text) => {
                outln!("lint `{id}`\n\n{text}");
                return Ok(());
            }
            None => return Err(anyhow!("no lint registered under `{id}`")),
        }
    }
    let files = collect_lint_targets(path)?;
    if files.is_empty() {
        return Err(anyhow!("no `.gos` sources found under {}", path.display()));
    }
    let mut warnings = 0usize;
    let mut errors = 0usize;
    let mut edits_applied = 0usize;
    let render_opts = gossamer_diagnostics::RenderOptions {
        colour: stderr_supports_colour(),
    };
    let linted: Vec<PathBuf> = files
        .iter()
        .map(|file| std::path::absolute(file).unwrap_or_else(|_| file.clone()))
        .collect();
    for file in files {
        let source = read_source(&file)?;
        let mut map = gossamer_lex::SourceMap::new();
        let file_id = map.add_file(file.to_string_lossy().into_owned(), source.clone());
        let (sf, parse_diags) = gossamer_parse::parse_source_file(&source, file_id);
        if !parse_diags.is_empty() {
            for diag in &parse_diags {
                let structured = diag.to_diagnostic();
                eprintln!(
                    "{}",
                    gossamer_diagnostics::render(&structured, &map, render_opts)
                );
            }
            errors += parse_diags.len();
            continue;
        }
        let mut registry = gossamer_lint::Registry::with_defaults();
        if deny_warnings {
            for (id, _) in registry.entries() {
                registry.set(id, gossamer_lint::Level::Deny);
            }
        }
        gossamer_lint::apply_attributes(&sf.attrs, &mut registry);
        if fix {
            let candidate_fixes = gossamer_lint::fixes(&sf, &registry, &source);
            if !candidate_fixes.is_empty() {
                let rewritten = gossamer_lint::apply_fixes(&source, &candidate_fixes);
                fs::write(&file, &rewritten)
                    .with_context(|| format!("write {}", file.display()))?;
                edits_applied += candidate_fixes.len();
                outln!(
                    "fix: {} edit(s) applied to {}",
                    candidate_fixes.len(),
                    file.display()
                );
            }
            continue;
        }
        let diagnostics = gossamer_lint::run(&sf, &source, &registry);
        for diag in &diagnostics {
            eprintln!("{}", gossamer_diagnostics::render(diag, &map, render_opts));
            match diag.severity {
                gossamer_diagnostics::Severity::Error => errors += 1,
                gossamer_diagnostics::Severity::Warning => warnings += 1,
                _ => {}
            }
        }
    }
    if fix {
        outln!("fix: {edits_applied} total edit(s) applied");
        return Ok(());
    }
    for entry in unit_entries(&linted) {
        for diag in typed_lint_diagnostics(&entry, &linted, deny_warnings, render_opts) {
            eprintln!("{}", diag.0);
            if diag.1 {
                errors += 1;
            } else {
                warnings += 1;
            }
        }
    }
    outln!("lint: {warnings} warning(s), {errors} error(s)");
    if errors > 0 {
        return Err(anyhow!("{errors} lint error(s)"));
    }
    Ok(())
}

/// The units the type-aware lints check to cover `files`: each directory's
/// `main.gos` or `lib.gos`, which bundles its sibling modules, or each file
/// on its own in a directory with neither.
fn unit_entries(files: &[PathBuf]) -> BTreeSet<PathBuf> {
    let mut entries = BTreeSet::new();
    for file in files {
        let dir = file.parent().unwrap_or_else(|| Path::new("."));
        let entry = ["main.gos", "lib.gos"]
            .iter()
            .map(|name| dir.join(name))
            .find(|candidate| candidate.is_file())
            .unwrap_or_else(|| file.clone());
        entries.insert(entry);
    }
    entries
}

/// The findings of the lints that read types (`gossamer_lint::TYPED_LINTS`)
/// over the unit `entry` bundles, rendered, for the spans that come from one
/// of the `linted` files. Each is paired with whether it counts as an error.
/// A unit that cannot be assembled reports nothing here: its files were
/// already linted for syntax, and `gos check` names what keeps it from
/// building.
fn typed_lint_diagnostics(
    entry: &Path,
    linted: &[PathBuf],
    deny_warnings: bool,
    render_opts: gossamer_diagnostics::RenderOptions,
) -> Vec<(String, bool)> {
    let Ok(unit) = crate::paths::read_entry_unit(entry) else {
        return Vec::new();
    };
    let prepared =
        crate::unit_cache::prepare(&unit.source, &entry.to_string_lossy(), &unit.origins);
    let source = prepared.source;
    let mut map = gossamer_lex::SourceMap::new();
    let file_id = map.add_file(entry.to_string_lossy().into_owned(), source.clone());
    crate::paths::register_unit_origins(&mut map, file_id, &unit.entry, &unit.origins);
    crate::paths::register_generated_tail(&mut map, file_id, prepared.generated_len);
    let outcome = gossamer_driver::check_frontend_with_program(&source, file_id, prepared.program);
    let typed_codes: Vec<&str> = gossamer_lint::TYPED_LINTS
        .iter()
        .map(|id| gossamer_lint::lint_code(id).as_str())
        .collect();
    let mut out = Vec::new();
    for diag in outcome.warnings {
        if !typed_codes.contains(&diag.code.as_str()) {
            continue;
        }
        let Some(label) = diag.labels.first() else {
            continue;
        };
        let (origin, _) = map.origin_of(label.location.span.file, label.location.span.start);
        let origin = Path::new(map.file_name(origin));
        if !linted.iter().any(|file| file == origin) {
            continue;
        }
        let mut diag = diag;
        if deny_warnings {
            diag.severity = gossamer_diagnostics::Severity::Error;
        }
        let is_error = diag.severity == gossamer_diagnostics::Severity::Error;
        out.push((
            gossamer_diagnostics::render(&diag, &map, render_opts),
            is_error,
        ));
    }
    out
}
