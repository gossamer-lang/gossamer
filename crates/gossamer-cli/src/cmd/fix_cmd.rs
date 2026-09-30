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
    let (sf, parses) = parse_reporting(file, &source);
    // A file that does not parse gives a plain rewriter nothing it can safely
    // act on; a repair still may, since it is kept only where it lowers the
    // diagnostic count.
    let selected: Vec<&Rewriter> = selected
        .iter()
        .copied()
        .filter(|r| parses || r.repair_only)
        .collect();
    let (rewritten, kept) = apply_rewriters(file, &source, &sf, &selected);
    if kept == 0 || rewritten == source {
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
    let (again, _) = parse_reporting(file, &rewritten);
    if apply_rewriters(file, &rewritten, &again, &selected).1 > 0 {
        return Err(anyhow!(
            "a rewriter is not idempotent on {}; no file was written",
            file.display()
        ));
    }
    if !check {
        fs::write(file, rewritten).map_err(|e| friendly_io_error(e, file))?;
    }
    Ok(kept)
}

/// Applies `selected` to `source`: every edit of a plain rewriter, then each
/// candidate edit of a repair-only rewriter that removes a diagnostic.
/// Returns the text and how many edits it kept.
fn apply_rewriters(
    file: &Path,
    source: &str,
    sf: &gossamer_ast::SourceFile,
    selected: &[&Rewriter],
) -> (String, usize) {
    let (repairs, plain): (Vec<&Rewriter>, Vec<&Rewriter>) =
        selected.iter().copied().partition(|r| r.repair_only);
    let fixes = migrations(sf, source, &plain);
    let mut kept = fixes.len();
    let mut current = if fixes.is_empty() {
        source.to_string()
    } else {
        gossamer_lint::apply_fixes(source, &fixes)
    };
    if repairs.is_empty() {
        return (current, kept);
    }
    // One repair per round, collected afresh from the text as it now stands,
    // so a candidate nested in another (`a.pow(2).pow(3)`) is never applied
    // at a stale offset.
    let mut baseline = diagnostics_for(file, &current);
    loop {
        let (tree, _) = parse_reporting(file, &current);
        let mut candidates = migrations(&tree, &current, &repairs);
        candidates.sort_by_key(|fix| std::cmp::Reverse(fix.span.start));
        let repaired = candidates.iter().find_map(|fix| {
            let candidate = gossamer_lint::apply_fixes(&current, std::slice::from_ref(fix));
            let count = diagnostics_for(file, &candidate);
            (count < baseline).then_some((candidate, count))
        });
        let Some((candidate, count)) = repaired else {
            break;
        };
        current = candidate;
        baseline = count;
        kept += 1;
    }
    (current, kept)
}

/// The tree for `source` and whether it parsed without error. A tree with
/// parse errors is still what a repair reads: a repair is kept only where it
/// lowers the diagnostic count, so it may fix the very error the parser
/// reported (GP0052, for one).
fn parse_reporting(file: &Path, source: &str) -> (gossamer_ast::SourceFile, bool) {
    let mut map = gossamer_lex::SourceMap::new();
    let id = map.add_file(file.to_string_lossy().into_owned(), source.to_string());
    let (sf, diags) = gossamer_parse::parse_source_file(source, id);
    (sf, diags.is_empty())
}

/// How far `candidate`, read as `file`'s text, is from checking: its parse
/// errors, then its front-end diagnostics. Parse errors rank first because
/// they hide every later phase: a repair that clears one may reveal
/// diagnostics the parse error had masked, and is still progress.
fn diagnostics_for(file: &Path, candidate: &str) -> (usize, usize) {
    let mut map = gossamer_lex::SourceMap::new();
    let raw_id = map.add_file(file.to_string_lossy().into_owned(), candidate.to_string());
    let parse_errors = gossamer_parse::parse_source_file(candidate, raw_id).1.len();
    let augmented = gossamer_parse::autoderive::augment_source(candidate);
    let Ok(folded) = crate::comptime_fold::fold_comptime(augmented, &file.to_string_lossy()) else {
        return (usize::MAX, usize::MAX);
    };
    let id = map.add_file(file.to_string_lossy().into_owned(), folded.clone());
    let total = gossamer_driver::check_frontend(&folded, id)
        .diagnostics
        .len();
    (parse_errors, total)
}
