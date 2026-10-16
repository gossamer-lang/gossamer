#![no_main]
//! Fuzz target: full front-end through name resolution + type
//! check.
//!
//! Drives the parsed `SourceFile` through `resolve_source_file`
//! and `typecheck_source_file`. The pass should never panic on
//! adversarial input: a malformed AST that the parser accepted
//! must surface as a structured diagnostic, not as a crash inside
//! the resolver or typechecker.

use libfuzzer_sys::fuzz_target;

use gossamer_lex::SourceMap;
use gossamer_parse::parse_source_file;
use gossamer_resolve::resolve_source_file;
use gossamer_types::{TyCtxt, typecheck_source_file};

fuzz_target!(|data: &[u8]| {
    // The lexer remains unsafe-free during fuzzing; do not invalidate global
    // symbols between callbacks.
    let Ok(source) = std::str::from_utf8(data) else {
        return;
    };
    if source.len() > 64 * 1024 {
        return;
    }
    let mut map = SourceMap::new();
    let file = map.add_file("fuzz.gos", source.to_string());
    let (mut sf, _diags) = parse_source_file(source, file);
    let (mut resolutions, _r_diags) = resolve_source_file(&sf);
    let _ = gossamer_types::normalize_caller_side_spellings(&mut sf, &mut resolutions);
    let mut tcx = TyCtxt::new();
    let (_table, _t_diags) = typecheck_source_file(&sf, &resolutions, &mut tcx);
});
