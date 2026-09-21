//! `gos fix [PATH] [--rewriter ID] [--list] [--check]` - applies the
//! toolchain's source migrations.
//!
//! Distinct from `gos lint --fix`, which acts on lints - observations
//! about the code the author wrote. A migration is a mechanical upgrade
//! the toolchain owns: the reader is not expected to have an opinion
//! about it, only to run it.
//!
//! Every rewrite is verified before it is kept. A file is re-parsed and
//! re-checked after rewriting, and the result is written only when the
//! file still checks with no more diagnostics than it started with. A
//! rewriter that would break a program cannot land one.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use gossamer_lint::migrate::{REWRITERS, Rewriter, migrations, rewriter};

use crate::paths::{collect_lint_targets, default_test_root, friendly_io_error, read_source};

/// Entry point for `gos fix`.
pub(crate) fn dispatch(
    path: Option<PathBuf>,
    rewriter_id: Option<String>,
    list: bool,
    check: bool,
) -> Result<()> {
    if list {
        outln!("Available rewriters:");
        for r in REWRITERS {
            let versions = if r.versions.is_empty() {
                "every version".to_string()
            } else {
                r.versions.join(", ")
            };
            outln!("  {:<28} {} [{versions}]", r.id, r.summary);
        }
        return Ok(());
    }

    let selected: Vec<&Rewriter> = match rewriter_id.as_deref() {
        Some(id) => vec![rewriter(id).ok_or_else(|| {
            anyhow!("unknown rewriter `{id}`; `gos fix --list` names the available ones")
        })?],
        None => REWRITERS.iter().collect(),
    };

    let resolved = match path {
        Some(p) => p,
        None => default_test_root()?,
    };
    let files = if resolved.is_file() {
        vec![resolved]
    } else {
        collect_lint_targets(&resolved)?
    };
    if files.is_empty() {
        return Err(anyhow!("no `.gos` sources found"));
    }

    let mut changed = 0usize;
    let mut edits = 0usize;
    for file in &files {
        match rewrite_file(file, &selected, check)? {
            0 => {}
            n => {
                changed += 1;
                edits += n;
                let verb = if check { "would rewrite" } else { "rewrote" };
                outln!("fix: {verb} {} ({n} edit(s))", file.display());
            }
        }
    }

    if check && changed > 0 {
        return Err(anyhow!(
            "{edits} pending migration(s) across {changed} file(s); run `gos fix`"
        ));
    }
    outln!(
        "fix: {edits} edit(s) across {changed} of {} file(s)",
        files.len()
    );
    Ok(())
}

/// Rewrites one file, returning how many edits were kept.
fn rewrite_file(file: &Path, selected: &[&Rewriter], check: bool) -> Result<usize> {
    let source = read_source(file)?;
    let Some(sf) = parse(file, &source) else {
        // A file that does not parse has nothing a rewriter can safely
        // act on; `gos check` is where the reader hears about it.
        return Ok(0);
    };
    let fixes = migrations(&sf, &source, selected);
    if fixes.is_empty() {
        return Ok(0);
    }
    let rewritten = gossamer_lint::apply_fixes(&source, &fixes);
    if rewritten == source {
        return Ok(0);
    }
    if diagnostics_for(file, &rewritten) > diagnostics_for(file, &source) {
        return Err(anyhow!(
            "migration of {} would introduce diagnostics; no file was written",
            file.display()
        ));
    }
    // Idempotence is a property of each rewriter, and re-running the pass
    // over its own output is the cheapest place to notice a lapse.
    if let Some(again) = parse(file, &rewritten)
        && !migrations(&again, &rewritten, selected).is_empty()
    {
        return Err(anyhow!(
            "a rewriter is not idempotent on {}; no file was written",
            file.display()
        ));
    }
    if !check {
        fs::write(file, rewritten).map_err(|e| friendly_io_error(e, file))?;
    }
    Ok(fixes.len())
}

fn parse(file: &Path, source: &str) -> Option<gossamer_ast::SourceFile> {
    let mut map = gossamer_lex::SourceMap::new();
    let id = map.add_file(file.to_string_lossy().into_owned(), source.to_string());
    let (sf, diags) = gossamer_parse::parse_source_file(source, id);
    diags.is_empty().then_some(sf)
}

/// Front-end diagnostic count for `candidate` read as `file`'s text.
fn diagnostics_for(file: &Path, candidate: &str) -> usize {
    let augmented = gossamer_parse::autoderive::augment_source(candidate);
    let Ok(folded) = crate::comptime_fold::fold_comptime(augmented, &file.to_string_lossy()) else {
        return usize::MAX;
    };
    let mut map = gossamer_lex::SourceMap::new();
    let id = map.add_file(file.to_string_lossy().into_owned(), folded.clone());
    gossamer_driver::check_frontend(&folded, id)
        .diagnostics
        .len()
}
