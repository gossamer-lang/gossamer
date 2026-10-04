//! Compile-time evaluation gate for the editor.
//!
//! `gos check` evaluates every `comptime` region on the bytecode VM and
//! splices the result back into the source before it reaches any
//! backend, so a region that is not compile-time-known is a hard error
//! there. Running the same fold here keeps the editor from calling a
//! file clean that the command-line gate rejects.
//!
//! The fold's spliced output is discarded: the editor's spans must stay
//! aligned with the buffer it holds, so only the pass/fail outcome is
//! consumed.

use gossamer_ast::SourceFile;
use gossamer_diagnostics::{Code, Diagnostic, Label, Location};
use gossamer_lex::{FileId, Span};
use gossamer_resolve::Resolutions;
use gossamer_types::{TyCtxt, TypeTable};

/// Names the source in the fold's location prefix. Free of `:` so the
/// `label:line:col` prefix parses back unambiguously.
const FOLD_LABEL: &str = "comptime";

/// Execution-path codes the compile-time VM can raise. A comptime
/// failure is a VM failure, so the message usually already names one of
/// these; carrying it through keeps the editor's code identical to the
/// one `gos check` prints for the same source.
const VM_CODES: [Code; 10] = [
    Code("GX0001"),
    Code("GX0002"),
    Code("GX0003"),
    Code("GX0004"),
    Code("GX0005"),
    Code("GX0006"),
    Code("GX0007"),
    Code("GX0008"),
    Code("GX0009"),
    Code("GX0010"),
];

/// Used when the evaluator's message names no code of its own: the
/// region reached something the compile-time VM cannot execute.
const FALLBACK_CODE: Code = Code("GX0007");

/// Evaluates every comptime region of `augmented` and returns a
/// diagnostic when a region is not compile-time-known or its result is
/// not spliceable. Returns `None` for a program with no `comptime`
/// spelling, which is the overwhelmingly common case and costs one
/// substring search.
///
/// `sf`, `resolutions`, and `types` must be the analysis of `augmented`
/// itself, and must already be free of fatal diagnostics: the fold
/// lowers the program to HIR, which is only meaningful for a program
/// that resolved and typechecked.
pub(crate) fn fold_diagnostic(
    uri: &str,
    augmented: &str,
    sf: &SourceFile,
    resolutions: &Resolutions,
    types: &TypeTable,
    tcx: &mut TyCtxt,
    file: FileId,
) -> Option<Diagnostic> {
    if !augmented.contains("comptime") {
        return None;
    }
    install_level(uri);
    let anchor = document_path(uri).map_or_else(
        || FOLD_LABEL.to_string(),
        |path| path.to_string_lossy().into_owned(),
    );
    let program = gossamer_hir::lower_source_file(sf, resolutions, types, tcx);
    let message = fold_on_vm_stack(program, tcx.clone(), augmented.to_string(), anchor).err()?;
    Some(to_diagnostic(&message, augmented, file))
}

/// Installs the compile-time capability level for a fold of the
/// document at `uri`.
///
/// The editor has no command line to read, so the level is the
/// `confined` default tightened by whatever the nearest
/// `project.comptime-io` asks for. Keeping the editor on the same
/// policy as `gos check` is what stops a file reading clean here and
/// failing there.
fn install_level(uri: &str) {
    use gossamer_runtime::comptime_policy::{self, ComptimeIo};
    let from_manifest = manifest_comptime_io(uri).and_then(|text| ComptimeIo::parse(&text));
    comptime_policy::set_level(comptime_policy::resolve(None, from_manifest));
}

/// The `project.comptime-io` of the nearest manifest above the document.
#[cfg(not(target_arch = "wasm32"))]
fn manifest_comptime_io(uri: &str) -> Option<String> {
    document_path(uri)
        .and_then(|path| nearest_manifest(&path))
        .and_then(|text| gossamer_pkg::Manifest::parse(&text).ok())
        .and_then(|manifest| manifest.project.comptime_io)
}

/// The browser editor has no project on disk.
#[cfg(target_arch = "wasm32")]
fn manifest_comptime_io(_uri: &str) -> Option<String> {
    None
}

/// Filesystem path a `file://` document URI names.
fn document_path(uri: &str) -> Option<std::path::PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    Some(std::path::PathBuf::from(rest))
}

/// Text of the nearest `project.toml` at or above `path`.
#[cfg(not(target_arch = "wasm32"))]
fn nearest_manifest(path: &std::path::Path) -> Option<String> {
    let mut dir = path.parent()?;
    loop {
        let candidate = dir.join("project.toml");
        if candidate.is_file() {
            return std::fs::read_to_string(candidate).ok();
        }
        dir = dir.parent()?;
    }
}

/// Runs the fold on a thread with the VM's native stack reserve, so the
/// language server's own stack never bounds comptime recursion depth,
/// and a panic inside the evaluator is reported instead of unwinding
/// through the request handler.
#[cfg(not(target_arch = "wasm32"))]
fn fold_on_vm_stack(
    program: gossamer_hir::HirProgram,
    tcx: TyCtxt,
    augmented: String,
    anchor: String,
) -> Result<String, String> {
    std::thread::Builder::new()
        .name("gos-lsp-comptime".to_string())
        .stack_size(gossamer_interp::VM_THREAD_STACK_BYTES)
        .spawn(move || {
            gossamer_interp::fold_into_source_anchored(
                &program, tcx, &augmented, FOLD_LABEL, &anchor,
            )
        })
        .map_err(|err| format!("comptime evaluation could not start: {err}"))?
        .join()
        .unwrap_or_else(|_| Err("comptime evaluation panicked".to_string()))
}

/// The wasm build has no threads, so the fold runs inline there.
#[cfg(target_arch = "wasm32")]
fn fold_on_vm_stack(
    program: gossamer_hir::HirProgram,
    tcx: TyCtxt,
    augmented: String,
    anchor: String,
) -> Result<String, String> {
    gossamer_interp::fold_into_source_anchored(&program, tcx, &augmented, FOLD_LABEL, &anchor)
}

/// Turns the fold's `comptime:LINE:COL: message` string into a located
/// diagnostic. A message without the prefix is anchored on the first
/// `comptime` spelling, the only region the program has to blame.
fn to_diagnostic(message: &str, augmented: &str, file: FileId) -> Diagnostic {
    let (offset, text) = split_location(message, augmented)
        .unwrap_or_else(|| (augmented.find("comptime").unwrap_or(0), message));
    let start = u32::try_from(offset).unwrap_or(u32::MAX);
    let end = start.saturating_add(if augmented[offset..].starts_with("comptime") {
        "comptime".len() as u32
    } else {
        1
    });
    Diagnostic::error(code_named_in(text), format!("comptime region: {text}"))
        .with_label(Label {
            location: Location::new(file, Span::new(file, start, end)),
            primary: true,
            message: None,
        })
        .with_note("comptime regions are evaluated during compilation and spliced back as literals, so every tier compiles the same constant")
}

/// The execution-path code `text` names in an `error[GX....]` prefix,
/// or the fallback when it names none.
fn code_named_in(text: &str) -> Code {
    VM_CODES
        .into_iter()
        .find(|code| text.contains(&format!("error[{}]", code.as_str())))
        .unwrap_or(FALLBACK_CODE)
}

/// Splits a `comptime:LINE:COL: message` prefix into a byte offset into
/// `augmented` and the remaining message text.
fn split_location<'a>(message: &'a str, augmented: &str) -> Option<(usize, &'a str)> {
    let rest = message.strip_prefix(FOLD_LABEL)?.strip_prefix(':')?;
    let (line, rest) = rest.split_once(':')?;
    let (column, text) = rest.split_once(": ")?;
    let offset = byte_offset(augmented, line.parse().ok()?, column.parse().ok()?)?;
    Some((offset, text))
}

/// Byte offset of a 1-based line and 1-based character column in
/// `source`. Character-counted because the fold reports columns in
/// characters, not bytes.
fn byte_offset(source: &str, line: usize, column: usize) -> Option<usize> {
    let mut start = 0usize;
    for _ in 1..line {
        start += source[start..].find('\n')? + 1;
    }
    let rest = &source[start..];
    let line_text = rest.split_once('\n').map_or(rest, |(text, _)| text);
    let byte = line_text
        .char_indices()
        .nth(column - 1)
        .map_or(line_text.len(), |(at, _)| at);
    Some(start + byte)
}

#[cfg(test)]
mod comptime_tests {
    use super::*;

    #[test]
    fn byte_offset_counts_characters_not_bytes() {
        let source = "let s = \"cafe\"\nlet t = 2\n";
        assert_eq!(byte_offset(source, 2, 1), Some(15));
        assert_eq!(byte_offset(source, 1, 9), Some(8));
    }

    #[test]
    fn a_program_without_comptime_is_never_folded() {
        let doc = crate::session::analyse("file:///plain.gos", "fn main() { }\n");
        assert!(
            doc.diagnostics
                .iter()
                .all(|diag| !diag.title.starts_with("comptime region:"))
        );
    }

    #[test]
    fn a_failing_comptime_region_is_reported_in_the_editor() {
        let source = "comptime fn tag() -> String { unimplemented() }\n\
                      fn main() { println(\"{}\", tag()) }\n";
        let doc = crate::session::analyse("file:///comptime.gos", source);
        let reported = doc
            .diagnostics
            .iter()
            .find(|diag| diag.title.starts_with("comptime region:"))
            .unwrap_or_else(|| panic!("unexpected diagnostics: {:?}", doc.diagnostics));
        assert_eq!(
            reported.code.as_str(),
            "GX0005",
            "the editor must carry the code `gos check` prints: {reported:?}"
        );
    }
}
