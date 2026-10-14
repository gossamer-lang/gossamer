//! Caches keyed by a unit's own text, ahead of every stage that rewrites it.
//!
//! The frontend cache keys its blob by the source the frontend reads, which
//! autoderive and the comptime fold produce, so a warm compile still paid
//! for both before it could look the blob up. A [`PreparedUnit`] caches their
//! output under the unit's own text instead. What a comptime region reads is
//! decided by running it, so those files are recorded with the entry and
//! compared with what they say now on every hit, as the build stamp does.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Error;
use serde::{Deserialize, Serialize};

/// A unit ready for the frontend: its text with autoderive's additions and
/// the comptime fold applied.
#[derive(Debug)]
pub(crate) struct PreparedUnit {
    /// The text the frontend reads.
    pub(crate) source: String,
    /// How many bytes at the end autoderive appended.
    pub(crate) generated_len: usize,
    /// The files the comptime fold read.
    pub(crate) comptime_inputs: Vec<PathBuf>,
    /// The fold's failure, with `source` left unfolded. A failed fold is
    /// never cached.
    pub(crate) fold_error: Option<Error>,
    /// Time spent in autoderive, zero when the unit came from the cache.
    pub(crate) autoderive: Duration,
    /// Time spent in the comptime fold, zero when the unit came from the
    /// cache.
    pub(crate) comptime: Duration,
    /// Whether the unit came from the cache.
    pub(crate) reused: bool,
    /// The tree autoderive parsed of the unit's own text, while `source`
    /// is the text it answered with that tree.
    pub(crate) program: Option<gossamer_parse::autoderive::ParsedProgram>,
}

#[derive(Serialize, Deserialize)]
struct PreparedBlob {
    generated_len: usize,
    inputs: Vec<String>,
    source: String,
}

/// Runs autoderive and the comptime fold over `user_source`, or answers
/// their earlier output for the same text when every file the fold read is
/// unchanged. `label` names the unit to the fold, as a relative read
/// resolves against it.
pub(crate) fn prepare(user_source: &str, label: &str) -> PreparedUnit {
    // A relative label resolves against the working directory, so the key
    // names the unit by its absolute path.
    let key = gossamer_driver::cache_enabled().then(|| {
        let anchored = std::path::absolute(label).map_or_else(
            |_| label.to_string(),
            |path| path.to_string_lossy().into_owned(),
        );
        gossamer_driver::unit_key(user_source, "prepared", &anchored)
    });
    if let Some(key) = &key
        && let Some(blob) = gossamer_driver::load_blob::<PreparedBlob>(key)
        && let Some(inputs) = inputs_unchanged(&blob.inputs)
    {
        gossamer_driver::trace_cache_hit("prepared unit reused", key);
        return PreparedUnit {
            source: blob.source,
            generated_len: blob.generated_len,
            comptime_inputs: inputs,
            fold_error: None,
            autoderive: Duration::ZERO,
            comptime: Duration::ZERO,
            reused: true,
            program: None,
        };
    }
    let started = Instant::now();
    let (augmented, program) = gossamer_parse::autoderive::augment_source_with_program(user_source);
    let generated_len = augmented.len().saturating_sub(user_source.len());
    let autoderive = started.elapsed();
    let started = Instant::now();
    let (source, fold_error) = match crate::comptime_fold::fold_comptime(augmented.clone(), label) {
        Ok(folded) => (folded, None),
        Err(err) => (augmented.clone(), Some(err)),
    };
    // A fold that spliced a constant changed the program's own text, which
    // the tree no longer describes.
    let program = program.filter(|_| source == augmented);
    let comptime_inputs = gossamer_runtime::comptime_inputs::take();
    let comptime = started.elapsed();
    if let Some(key) = &key
        && fold_error.is_none()
        && let Some(inputs) = input_lines(&comptime_inputs)
    {
        gossamer_driver::store_blob(
            key,
            &PreparedBlob {
                generated_len,
                inputs,
                source: source.clone(),
            },
        );
    }
    PreparedUnit {
        source,
        generated_len,
        comptime_inputs,
        fold_error,
        autoderive,
        comptime,
        reused: false,
        program,
    }
}

/// How a recorded compile-time input is spelled in a stamp:
/// `input <len> <mtime> <path>`, path last so it needs no escaping.
pub(crate) const COMPTIME_INPUT_PREFIX: &str = "input ";

/// The stamp line for compile-time input `path`, or `None` when the path
/// cannot be stated on one line.
///
/// A path holding a newline is the one shape a line-oriented stamp cannot
/// express; refusing to record it makes every later lookup a miss, which is
/// the safe direction.
pub(crate) fn comptime_input_line(path: &Path) -> Option<String> {
    let text = path.to_string_lossy();
    if text.contains('\n') {
        return None;
    }
    let (len, mtime) = fs::metadata(path).map_or_else(
        |_| ("-".to_string(), "-".to_string()),
        |meta| {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or_else(|| "-".to_string(), |since| since.as_nanos().to_string());
            (meta.len().to_string(), mtime)
        },
    );
    Some(format!("{COMPTIME_INPUT_PREFIX}{len} {mtime} {text}"))
}

/// The stamp lines for `paths`, or `None` when one cannot be stated.
pub(crate) fn input_lines(paths: &[PathBuf]) -> Option<Vec<String>> {
    paths.iter().map(|path| comptime_input_line(path)).collect()
}

/// The recorded inputs' paths when each line still describes its file.
pub(crate) fn inputs_unchanged(lines: &[String]) -> Option<Vec<PathBuf>> {
    let mut paths = Vec::with_capacity(lines.len());
    for line in lines {
        let path = line
            .strip_prefix(COMPTIME_INPUT_PREFIX)?
            .split_once(' ')?
            .1
            .split_once(' ')?
            .1;
        if comptime_input_line(Path::new(path))? != *line {
            return None;
        }
        paths.push(PathBuf::from(path));
    }
    Some(paths)
}
