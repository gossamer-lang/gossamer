//! `gos lint [PATH] [--deny-warnings] [--explain ID] [--fix]` -
//! runs the lint suite and applies / reports findings.

use std::fs;
use std::path::PathBuf;

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
    outln!("lint: {warnings} warning(s), {errors} error(s)");
    if errors > 0 {
        return Err(anyhow!("{errors} lint error(s)"));
    }
    Ok(())
}
